// What files/data/trustiot-data-setup decides when the TPM does not open the data volume
// (Laysi/trustiot-v2#867; README.md, section 「TPM 永久解不開時」): its functions, sourced into
// bash with the TPM and cryptsetup replaced by stand-ins, so the decision is tested without a TPM.
// That the stand-ins answer the way the real programs do is the boot test's part.
import { afterAll, describe, expect, test } from "bun:test";
import { existsSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
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

/**
 * A copy of the setup script whose FACTORY_FLAG (a constant, on /run) is FLAG in the test's
 * directory, for the tests of what writes it.
 */
const FLAG = join(dir, "back-to-factory");
const FLAG_LINE = `readonly FACTORY_FLAG=${DATA_CHECK.factoryFlag}\n`;
const SCRIPT_WITH_FLAG = join(dir, "trustiot-data-setup");
{
  const text = readFileSync(SCRIPT, "utf8");
  if (!text.includes(FLAG_LINE)) throw new Error(`the setup script has no line ${JSON.stringify(FLAG_LINE)}`);
  writeFileSync(SCRIPT_WITH_FLAG, text.replace(FLAG_LINE, `readonly FACTORY_FLAG=${FLAG}\n`));
}

/** Runs `body` in bash after sourcing the setup script (which must not run main() then). */
async function sourced(body: string, script = SCRIPT): Promise<{ stdout: string; stderr: string; code: number }> {
  const child = Bun.spawn(["bash", "-c", `source "$1"\n${body}`, "data-setup-test", script], { stdout: "pipe", stderr: "pipe", cwd: dir });
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
    for (const between of ["absent", "silent", "unrecorded", "unknown-owner", "same-owner", "unknown-keys", "keyed 0x81000100", `other-owner ${C}`]) {
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

  for (const answer of ["absent", "silent", "unrecorded", "unknown-owner", "same-owner", "unknown-keys", "keyed 0x81000100"]) {
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
async function observe(stub: {
  present?: boolean;
  opens?: boolean;
  answers?: boolean;
  token?: string | null;
  owner?: string | null;
  srk?: string | null;
  /** What `tpm2_getcap handles-persistent` prints (null: it fails); none by default. */
  handles?: string | null;
}): Promise<string> {
  const token = stub.token === undefined ? `{"type":"${LUKS.ownerToken.type}","keyslots":[],"owner":"${A}"}` : stub.token;
  const handles = stub.handles === undefined ? "" : stub.handles;
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
tpm2_getcap() {
	case $1 in
	properties-variable) ${stub.answers === false ? "return 1" : "return 0"} ;;
	handles-persistent) ${handles === null ? "return 1" : `printf '%s' ${quote(handles)}`} ;;
	*) echo "unexpected tpm2_getcap $*" >&2; return 99 ;;
	esac
}
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

  test("another owner hierarchy in a TPM without keys: another TPM, or this one cleared", async () => {
    expect(await observe({ owner: B })).toBe(`other-owner ${B}`);
    // This owner hierarchy's own standard storage key (systemd's, from an earlier boot of this
    // TPM), and the platform's own objects, are not keys.
    expect(await observe({ owner: B, srk: B, handles: "- 0x81000001\n" })).toBe(`other-owner ${B}`);
    expect(await observe({ owner: B, handles: "- 0x81800000\n- 0x81FFFFFF\n" })).toBe(`other-owner ${B}`);
  });

  test("another owner hierarchy in a TPM that holds keys: another gateway's TPM, never a wipe", async () => {
    expect(await observe({ owner: B, srk: B, handles: "- 0x81000001\n- 0x81000100\n" })).toBe("keyed 0x81000100");
    // An endorsement key a vendor or a tool persisted counts too: whatever is less certain stays locked.
    expect(await observe({ owner: B, handles: "- 0x81000100\n- 0x81010001\n- 0x817FFFFF\n- 0x81800001\n" })).toBe("keyed 0x81000100,0x81010001,0x817fffff");
    expect(await observe({ owner: B, handles: "- 0x81000000\n" })).toBe("keyed 0x81000000");
  });

  test("a key at 0x81000001 that is not this owner hierarchy's standard storage key is a key: a wipe would seal under it", async () => {
    expect(await observe({ owner: B, srk: C, handles: "- 0x81000001\n" })).toBe("keyed 0x81000001");
    expect(await observe({ owner: B, srk: C, handles: "- 0x81000001\n- 0x81000100\n" })).toBe("keyed 0x81000001,0x81000100");
  });

  test("another owner hierarchy, but the persistent handles cannot be read: never a wipe", async () => {
    expect(await observe({ owner: B, handles: null })).toBe("unknown-keys");
    for (const handles of ["- 0x8100010\n", "0x81000100\n", "- 0x81000100 \n", "- 0x81000100\nsomething else\n", "- 0x8100010g\n", "-  0x81000100\n"]) {
      expect({ handles, answer: await observe({ owner: B, handles }) }).toEqual({ handles, answer: "unknown-keys" });
    }
    // 0x81000001 is listed, but the name of what it holds cannot be read.
    expect(await observe({ owner: B, srk: null, handles: "- 0x81000001\n" })).toBe("unknown-keys");
  });

  test("the persistent handles are read only when the owner hierarchy is another one", async () => {
    // A listing it would refuse makes no difference to the other answers.
    expect(await observe({ owner: A, handles: null })).toBe("same-owner");
    expect(await observe({ owner: B, srk: A, handles: null })).toBe("same-owner");
    expect(await observe({ owner: null, handles: null })).toBe("unknown-owner");
  });

  test("cryptsetup's JSON with spaces is read the same", async () => {
    expect(await observe({ token: `{ "type": "${LUKS.ownerToken.type}", "keyslots": [ ], "owner": "${A}" }`, owner: B })).toBe(`other-owner ${B}`);
  });
});

/** check_or_wipe with check_tpm deciding `verdict`: whether it formats, with which mark, and what it logs. */
async function decide(verdict: string): Promise<{ formatted: string | null; log: string }> {
  const r = await sourced(`
check_tpm() { verdict=${quote(verdict)}; }
recorded_owner() { echo ${A}; }
format() { printf 'format %s %s\\n' "$1" "$2"; }
check_or_wipe /dev/test-data
`);
  if (r.code !== 0) throw new Error(`bash exited with ${r.code}: ${r.stderr}`);
  const line = r.stdout.split("\n").find((l) => l.startsWith("format "));
  return { formatted: line ?? null, log: r.stderr };
}

describe("trustiot-data-setup: what it does with the verdict", () => {
  test("opens: nothing is formatted", async () => {
    expect(await decide("opens")).toEqual({ formatted: null, log: "/dev/test-data is encrypted and opens only with the TPM\n" });
  });

  test("permanent: wiped and made anew, with the mark naming both owner hierarchies", async () => {
    const r = await decide(`permanent ${B}`);
    expect(r.formatted).toBe(`format /dev/test-data {"type":"${LUKS.markToken.type}","keyslots":[],"reason":"tpm-owner-changed","sealed_owner":"${A}","found_owner":"${B}"}`);
    expect(r.log).toContain("wiping /dev/test-data and making it anew for this TPM");
  });

  test("unsure: never formatted, and the journal says why", async () => {
    for (const last of ["absent", "silent", "unrecorded", "unknown-owner", "same-owner", "unknown-keys", `other-owner ${B}`]) {
      const r = await decide(`unsure ${last}`);
      expect({ last, ...r }).toEqual({ last, formatted: null, log: `<4>the TPM did not open /dev/test-data within ${DATA_CHECK.windowS} s (last: ${last}); not wiping it: it stays locked\n` });
    }
  });

  test("unsure because the TPM holds keys: the journal says the card is most likely another gateway's", async () => {
    const r = await decide("unsure keyed 0x81000100");
    expect(r.formatted).toBeNull();
    expect(r.log.split("\n")).toEqual([
      `<4>the TPM did not open /dev/test-data within ${DATA_CHECK.windowS} s (last: keyed 0x81000100); not wiping it: it stays locked`,
      "<4>this TPM holds keys (0x81000100) but is not the one the volume was sealed under: most likely another gateway's TPM, so this card is not this gateway's own",
      "",
    ]);
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

/**
 * What `cryptsetup luksDump` prints (cryptsetup 2.7, shortened) for a header with this label and
 * these tokens ("<id>: <type>"); the TPM's token has its lines below it, as cryptsetup prints them.
 */
function luksDump(tokens: string[], label: string = LUKS.label): string {
  return `${[
    "LUKS header information",
    "Version:       \t2",
    `UUID:          \t${LUKS.uuid}`,
    `Label:         \t${label}`,
    "Subsystem:     \t(no subsystem)",
    "",
    "Keyslots:",
    "  1: luks2",
    "\tKey:        512 bits",
    "\tDigest ID:  0",
    "Tokens:",
    ...tokens.flatMap((t) => [`  ${t}`, ...(t.endsWith(" systemd-tpm2") ? ["\ttpm2-pcr-bank:    n/a", "\ttpm2-blob: 0123 4567", "\t           89ab", "\tKeyslot:    1"] : [])]),
    "Digests:",
    "  0: pbkdf2",
    "\tHash:       sha256",
  ].join("\n")}\n`;
}

const MARK = `{"type":"${LUKS.markToken.type}","keyslots":[],"reason":"tpm-owner-changed","sealed_owner":"${A}","found_owner":"${B}"}`;
const TOKENS = [`0: systemd-tpm2`, `${LUKS.ownerToken.id}: ${LUKS.ownerToken.type}`];
const MARKED = [...TOKENS, `${LUKS.markToken.id}: ${LUKS.markToken.type}`];

/**
 * `body` after sourcing `script`, with `cryptsetup luksDump` printing `dump` and `cryptsetup
 * token export` of the mark's id printing `token` (null: it fails).
 */
async function withHeader(stub: { dump: string | null; token?: string | null }, body: string, script = SCRIPT) {
  return sourced(
    `
${stub.dump === null ? "unset DUMP" : `DUMP=${quote(stub.dump)}`}
${stub.token == null ? "unset TOKEN" : `TOKEN=${quote(stub.token)}`}
cryptsetup() {
	case $1 in
	luksDump) [[ -n \${DUMP+x} && $2 == /dev/test-data ]] || return 1; printf '%s' "$DUMP" ;;
	token) [[ "$2 $3 $4 $5" == "export --token-id ${LUKS.markToken.id} /dev/test-data" && -n \${TOKEN+x} ]] || return 1; printf '%s\\n' "$TOKEN" ;;
	*) echo "unexpected cryptsetup $*" >&2; return 99 ;;
	esac
}
${body}
`,
    script,
  );
}

describe("trustiot-data-setup: reading the header", () => {
  test("header_tokens lists each token's id and type, and nothing from the other sections", async () => {
    const r = await withHeader({ dump: luksDump(MARKED) }, "header_tokens /dev/test-data");
    expect(r.stdout.trim().split("\n")).toEqual(["0 systemd-tpm2", `${LUKS.ownerToken.id} ${LUKS.ownerToken.type}`, `${LUKS.markToken.id} ${LUKS.markToken.type}`]);
    expect((await withHeader({ dump: luksDump([]) }, "header_tokens /dev/test-data; echo \"exit $?\"")).stdout).toBe("exit 0\n");
    expect((await withHeader({ dump: null }, "header_tokens /dev/test-data || echo \"exit $?\"")).stdout).toBe("exit 1\n");
  });

  test("luks_label is the label cryptsetup reads", async () => {
    expect((await withHeader({ dump: luksDump(TOKENS, LUKS.labelNew) }, "luks_label /dev/test-data")).stdout).toBe(`${LUKS.labelNew}\n`);
    expect((await withHeader({ dump: luksDump(TOKENS) }, "luks_label /dev/test-data")).stdout).toBe(`${LUKS.label}\n`);
    expect((await withHeader({ dump: null }, "luks_label /dev/test-data || echo \"exit $?\"")).stdout).toBe("exit 1\n");
  });

  test("has_mark: only a token of the mark's type with the mark's id; a header it cannot read is not one without a mark", async () => {
    const cases: [string, string | null, number][] = [
      ["the mark", luksDump(MARKED), 0],
      ["no tokens", luksDump([]), 1],
      ["the TPM's and the fingerprint's tokens only", luksDump(TOKENS), 1],
      ["another type with the mark's id", luksDump([...TOKENS, `${LUKS.markToken.id}: something-else`]), 1],
      ["the mark's type with another id", luksDump([...TOKENS, `3: ${LUKS.markToken.type}`]), 1],
      ["luksDump fails", null, 2],
    ];
    for (const [what, dump, code] of cases) {
      const r = await withHeader({ dump }, 'code=0; has_mark /dev/test-data || code=$?; echo "$code"');
      expect({ what, code: Number(r.stdout.trim()) }).toEqual({ what, code });
    }
  });

  test("read_mark: nothing without a mark, the mark made again with one, and a failure when it cannot tell", async () => {
    const cases: [string, { dump: string | null; token?: string | null }, string][] = [
      ["no mark", { dump: luksDump(TOKENS) }, "[] exit 0"],
      ["another type with the mark's id", { dump: luksDump([...TOKENS, `${LUKS.markToken.id}: something-else`]), token: '{"type":"something-else","keyslots":[]}' }, "[] exit 0"],
      ["the mark", { dump: luksDump(MARKED), token: MARK }, `[${MARK}] exit 0`],
      ["luksDump fails", { dump: null, token: MARK }, "[] exit 1"],
      ["the mark is listed but cannot be exported", { dump: luksDump(MARKED), token: null }, "[] exit 1"],
      ["the listing and the token disagree", { dump: luksDump(MARKED), token: '{"type":"something-else","keyslots":[]}' }, "[] exit 1"],
    ];
    for (const [what, stub, expected] of cases) {
      const r = await withHeader(stub, 'code=0; out=$(read_mark /dev/test-data) || code=$?; echo "[$out] exit $code"');
      expect({ what, out: r.stdout.trim() }).toEqual({ what, out: expected });
    }
  });
});

describe("trustiot-data-setup: the mark at every boot (announce_mark)", () => {
  /** announce_mark on a copy of the script whose flag is FLAG (left over from an earlier boot when `stale`). */
  async function announce(stub: { dump: string | null; token?: string | null }, stale = true) {
    rmSync(FLAG, { force: true });
    if (stale) writeFileSync(FLAG, "reason=stale\n");
    const r = await withHeader(stub, "announce_mark /dev/test-data", SCRIPT_WITH_FLAG);
    return { code: r.code, flag: existsSync(FLAG) ? readFileSync(FLAG, "utf8") : null, log: r.stderr };
  }

  test("no mark: no flag, also when an earlier boot left one", async () => {
    expect(await announce({ dump: luksDump(TOKENS) })).toEqual({ code: 0, flag: null, log: "" });
  });

  test("the mark: the flag names it, readable by anyone, and the journal says so", async () => {
    const r = await announce({ dump: luksDump(MARKED), token: MARK });
    expect(r.code).toBe(0);
    expect(r.flag).toBe(`reason=tpm-owner-changed\nsealed_owner=${A}\nfound_owner=${B}\n`);
    expect((await Bun.$`stat -c %a ${FLAG}`.text()).trim()).toBe("644");
    expect(r.log).toContain("carries the mark to go back to the factory: the agent stays off");
  });

  test("whether the mark is there cannot be told: the flag keeps the agent off and the setup fails", async () => {
    const unknown = "reason=unknown\nsealed_owner=unknown\nfound_owner=unknown\n";
    for (const stub of [{ dump: null }, { dump: luksDump(MARKED), token: null }]) {
      const r = await announce(stub, false);
      expect({ code: r.code, flag: r.flag }).toEqual({ code: 1, flag: unknown });
      expect(r.log).toContain("refusing: cannot read whether /dev/test-data carries the mark to go back to the factory");
    }
  });
});

/** sealing_owner with the fingerprint, the persistent handles and the name at 0x81000001 replaced (null: it fails). */
async function sealing(stub: { owner: string | null; handles: string | null; srk?: string | null; expected?: string }): Promise<{ out: string; log: string }> {
  const r = await sourced(`
owner_fingerprint() { ${stub.owner === null ? "return 1" : `echo ${stub.owner}`}; }
owner_handles() { ${stub.handles === null ? "return 1" : `echo ${quote(stub.handles)}`}; }
srk_name() { ${stub.srk ? `echo ${stub.srk}` : "return 1"}; }
if fingerprint=$(sealing_owner ${quote(dir)} ${quote(stub.expected ?? "")}); then echo "seals under $fingerprint"; else echo refused; fi
`);
  if (r.code !== 0) throw new Error(`bash exited with ${r.code}: ${r.stderr}`);
  return { out: r.stdout.trim(), log: r.stderr };
}

describe("trustiot-data-setup: what format() seals the volume under (sealing_owner)", () => {
  test("a TPM without persistent keys, or with this owner hierarchy's standard storage key at 0x81000001", async () => {
    expect(await sealing({ owner: A, handles: "" })).toEqual({ out: `seals under ${A}`, log: "" });
    expect(await sealing({ owner: A, handles: "0x81000001", srk: A })).toEqual({ out: `seals under ${A}`, log: "" });
    // A key elsewhere is not what the volume key is sealed under.
    expect(await sealing({ owner: A, handles: "0x81000100" })).toEqual({ out: `seals under ${A}`, log: "" });
  });

  test("another key at 0x81000001, or one it cannot read: never sealed under it", async () => {
    const reason = "0x81000001 holds a key that is not this TPM's standard storage key, or it cannot be read: the volume key would be sealed under it\n";
    expect(await sealing({ owner: A, handles: "0x81000001", srk: C })).toEqual({ out: "refused", log: reason });
    expect(await sealing({ owner: A, handles: "0x81000001,0x81000100", srk: C })).toEqual({ out: "refused", log: reason });
    expect(await sealing({ owner: A, handles: "0x81000001", srk: null })).toEqual({ out: "refused", log: reason });
    expect(await sealing({ owner: A, handles: null })).toEqual({ out: "refused", log: reason });
  });

  test("the owner hierarchy cannot be read", async () => {
    expect(await sealing({ owner: null, handles: "" })).toEqual({ out: "refused", log: "cannot read the TPM's owner hierarchy (tpm2_createprimary)\n" });
  });

  test("a wipe seals only under the owner hierarchy its check found", async () => {
    expect(await sealing({ owner: B, handles: "", expected: B })).toEqual({ out: `seals under ${B}`, log: "" });
    expect(await sealing({ owner: C, handles: "", expected: B })).toEqual({ out: "refused", log: `the TPM's owner hierarchy is ${C} now, not ${B} as when it was checked\n` });
  });
});

/**
 * main() on a partition whose first metadata copy (blkid) says a setup did not finish, with
 * cryptsetup reading `label` and read_mark printing `mark` (null: either fails); format() only
 * says what it was asked. On the copy of the script whose flag is FLAG.
 */
async function startAgain(stub: { label: string | null; mark: string | null }): Promise<{ code: number; formatted: string | null; flag: string | null; log: string }> {
  rmSync(FLAG, { force: true });
  const r = await sourced(
    `
data_partition() { echo /dev/test-data; }
probe() { case $2 in TYPE) echo crypto_LUKS ;; UUID) echo ${LUKS.uuid} ;; LABEL) echo ${LUKS.labelNew} ;; esac; }
mktemp() { command mktemp -d ${quote(join(dir, "run.XXXXXXXX"))}; }
luks_label() { ${stub.label === null ? "return 1" : `echo ${quote(stub.label)}`}; }
read_mark() { ${stub.mark === null ? "return 1" : `printf '%s' ${quote(stub.mark)}`}; }
format() { printf 'format %s [%s]\\n' "$1" "$2"; }
unique_uuid() { :; }
announce_mark() { :; }
main
`,
    SCRIPT_WITH_FLAG,
  );
  const line = r.stdout.split("\n").find((l) => l.startsWith("format "));
  return { code: r.code, formatted: line ?? null, flag: existsSync(FLAG) ? readFileSync(FLAG, "utf8") : null, log: r.stderr };
}

const UNKNOWN_FLAG = "reason=unknown\nsealed_owner=unknown\nfound_owner=unknown\n";

describe("trustiot-data-setup: a setup that did not finish is done again only as it was", () => {
  test("both metadata copies say it did not finish: formatted again, with the mark when it had one", async () => {
    expect(await startAgain({ label: LUKS.labelNew, mark: "" })).toMatchObject({ code: 0, formatted: "format /dev/test-data []", flag: null });
    expect(await startAgain({ label: LUKS.labelNew, mark: MARK })).toMatchObject({ code: 0, formatted: `format /dev/test-data [${MARK}]`, flag: null });
  });

  test("cryptsetup reads another label (the two copies differ) or none: never formatted, and the agent stays off", async () => {
    for (const label of [LUKS.label, "", null]) {
      const r = await startAgain({ label, mark: "" });
      expect({ label, code: r.code, formatted: r.formatted, flag: r.flag }).toEqual({ label, code: 1, formatted: null, flag: UNKNOWN_FLAG });
      expect(r.log).toContain("the header's two copies differ; not formatting it");
    }
  });

  test("whether it carries the mark cannot be told: never formatted, so the mark is never lost, and the agent stays off", async () => {
    const r = await startAgain({ label: LUKS.labelNew, mark: null });
    expect({ code: r.code, formatted: r.formatted, flag: r.flag }).toEqual({ code: 1, formatted: null, flag: UNKNOWN_FLAG });
    expect(r.log).toContain("cannot read whether /dev/test-data carries the mark to go back to the factory: not making it again");
  });
});
