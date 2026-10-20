// The rules of scripts/demo/test-mail.ts, without the demo machine, ssh or a mail relay
// (Laysi/trustiot-v2#468). test-mail.container.test.ts runs the pod's script in the pinned image.
import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { mkdirSync, mkdtempSync, readdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { tryRun, type RunResult } from "../release/exec.ts";
import { Box, DEMO_NAMESPACE, Log, ROOT } from "./lib.ts";
import {
  CODE_PATHS,
  IMAGE,
  ISSUE,
  OUTPUT_FILTER,
  POD_LABELS,
  POD_PREFIX,
  POD_SCRIPT,
  RECIPIENT_PROMPT,
  SECRET_KEYS_TEMPLATE,
  checkedRecipient,
  checkoutSettings,
  filterOutput,
  headerSummary,
  mailOutcome,
  makePlan,
  networkPolicyFacts,
  parseAuthResults,
  podManifest,
  readCheckout,
  receivedTls,
  relation,
  report,
  runId,
  selects,
  sendTestMail,
  senderSource,
  smtpSettings,
  type Checkout,
  type SendPlan,
  type SmtpSettings,
} from "./test-mail.ts";

/** The chart's default key names in the SMTP Secret (deploy/chart/values.yaml, smtp.credentialsSecret). */
const KEYS = (Bun.YAML.parse(readFileSync(join(ROOT, "deploy/chart/values.yaml"), "utf8")) as { smtp: { credentialsSecret: { usernameKey: string; passwordKey: string } } }).smtp.credentialsSecret;
const credentials = (name: string) => ({ name, usernameKey: KEYS.usernameKey, passwordKey: KEYS.passwordKey });

const RECIPIENT = "someone@inbox.example.com";
const SMTP: SmtpSettings = { host: "smtp.example.com", port: 587, security: "starttls", credentialsSecret: credentials("trustiot-smtp") };
const SENDER = { name: "trustiot-demo-values", key: "smtp-from" };

let dir: string;
let log: Log;
beforeEach(() => {
  dir = mkdtempSync(join(tmpdir(), "demo-test-mail-"));
  log = new Log("test", join(dir, "log"));
});
afterEach(() => rmSync(dir, { recursive: true, force: true }));

const ok = (stdout = ""): RunResult => ({ stdout, stderr: "", exitCode: 0 });
const notFound = (what: string): RunResult => ({ stdout: "", stderr: `Error from server (NotFound): ${what} not found`, exitCode: 1 });
const fakeBox = (handler: (command: string, stdin?: string) => RunResult) =>
  new Box("demo-box", log, { run: async (cmd, stdin) => handler(cmd.at(-1) ?? "", typeof stdin === "string" ? stdin : stdin === undefined ? undefined : new TextDecoder().decode(stdin)), sleep: async () => {} });
const logText = () =>
  readdirSync(join(dir, "log"))
    .map((name) => readFileSync(join(dir, "log", name), "utf8"))
    .join("\n");

describe("test-mail.ts: the checkout's settings", () => {
  test("reads the demo release's SMTP settings and the sender's Secret from the repository", () => {
    const checkout = checkoutSettings((path) => readFileSync(join(ROOT, path), "utf8"), "");
    const demo = Bun.YAML.parse(readFileSync(join(ROOT, "deploy/flux/clusters/demo/trustiot/values-demo.yaml"), "utf8")) as { smtp: { host: string } };
    expect(checkout.smtp).toEqual({ host: demo.smtp.host, port: 587, security: "starttls", credentialsSecret: credentials("trustiot-smtp") });
    expect(checkout.sender).toEqual(SENDER);
  });

  test("layers override in Helm's order, and the chart's key names stay unless a layer sets them", () => {
    const chart = { host: "", port: 587, security: "starttls", from: "", credentialsSecret: credentials("") };
    expect(smtpSettings([chart, {}, { host: "smtp.example.com", port: 465, security: "tls", credentialsSecret: { name: "relay" } }])).toEqual({
      host: "smtp.example.com",
      port: 465,
      security: "tls",
      credentialsSecret: credentials("relay"),
    });
  });

  test("refuses what the chart would not send with: no host, no port, plain SMTP, no credentials", () => {
    const base = { host: "smtp.example.com", port: 587, security: "starttls", credentialsSecret: credentials("relay") };
    const cases: [Record<string, unknown>, RegExp][] = [
      [{ host: "" }, /smtp.host is not set/],
      [{ host: "SMTP.Example.com" }, /lowercase DNS name/],
      [{ port: "587" }, /not a TCP port/],
      [{ port: 70_000 }, /not a TCP port/],
      [{ security: "none" }, /starttls or tls/],
      [{ security: undefined }, /starttls or tls/],
      [{ credentialsSecret: credentials("") }, /needs credentials/],
    ];
    for (const [change, error] of cases) expect(() => smtpSettings([{ ...base, ...change }])).toThrow(error);
  });

  test("the sender must come from exactly one Secret key that the HelmRelease writes into smtp.from", () => {
    const entry = { kind: "Secret", name: "trustiot-demo-values", valuesKey: "smtp-from", targetPath: "smtp.from", literal: true };
    expect(senderSource({ spec: { valuesFrom: [{ kind: "ConfigMap", name: "x", valuesKey: "y" }, entry] } })).toEqual(SENDER);
    expect(() => senderSource({ spec: { valuesFrom: [] } })).toThrow(/exactly one/);
    expect(() => senderSource({ spec: { valuesFrom: [entry, entry] } })).toThrow(/exactly one/);
    expect(() => senderSource({ spec: { valuesFrom: [{ ...entry, kind: "ConfigMap" }] } })).toThrow(/key of a Secret/);
  });

  test("a checkout with uncommitted changes in the plan's paths is refused", async () => {
    const repo = join(dir, "repo");
    for (const path of ["scripts/demo", "deploy/flux/clusters/demo/trustiot", "deploy/chart"]) mkdirSync(join(repo, path), { recursive: true });
    for (const file of ["deploy/chart/values.yaml", "deploy/flux/clusters/demo/trustiot/values-saas.yaml", "deploy/flux/clusters/demo/trustiot/values-demo.yaml", "deploy/flux/clusters/demo/trustiot/helm-release.yaml"]) {
      writeFileSync(join(repo, file), readFileSync(join(ROOT, file), "utf8"));
    }
    writeFileSync(join(repo, "scripts/demo/x.ts"), "// x\n");
    const git = (...args: string[]) => tryRun(["git", "-C", repo, "-c", "user.name=t", "-c", "user.email=t@example.com", "-c", "commit.gpgsign=false", ...args]);
    await git("init", "-q");
    await git("add", ".");
    expect((await git("commit", "-q", "-m", "x")).exitCode).toBe(0);
    const clean = await readCheckout(repo);
    expect(clean.head).toMatch(/^[0-9a-f]{40}$/);
    for (const path of CODE_PATHS) {
      const file = path.endsWith(".yaml") ? join(repo, path) : join(repo, path, "new.txt");
      writeFileSync(file, `${path.endsWith(".yaml") ? readFileSync(file, "utf8") : ""}# changed\n`);
      await expect(readCheckout(repo)).rejects.toThrow(/uncommitted changes/);
      await git("checkout", "-q", "--", ".");
      await git("clean", "-qfd");
    }
  });
});

describe("test-mail.ts: the recipient", () => {
  test("one plain address on one line; errors never quote the input", () => {
    expect(checkedRecipient(`${RECIPIENT}\n`)).toBe(RECIPIENT);
    expect(checkedRecipient(`  ${RECIPIENT}\r\n\n`)).toBe(RECIPIENT);
    expect(checkedRecipient("first.last+demo@sub.inbox.example.com")).toBe("first.last+demo@sub.inbox.example.com");
    for (const bad of ["", "\n", `${RECIPIENT}\nsecond@example.com`, "Some One <someone@example.com>", "a$(id)@example.com", "a@b", "a..b@example.com", ".a@example.com", "a@example.com>,<b@example.com", "a b@example.com", "\"a\"@example.com", "a@-example.com"]) {
      let message = "";
      try {
        checkedRecipient(bad);
      } catch (error) {
        message = String(error);
      }
      expect(message).not.toBe("");
      if (bad.trim() !== "") expect(message).not.toContain(bad.trim().split("\n")[0]!);
    }
  });

  // A child process on a pseudo-terminal: the prompt path of readRecipient, as an operator runs it.
  const TERMINAL_WAIT_MS = 60_000;
  async function waitFor(done: () => boolean, what: string): Promise<void> {
    const deadline = Date.now() + TERMINAL_WAIT_MS;
    while (!done()) {
      if (Date.now() > deadline) throw new Error(`timed out after ${TERMINAL_WAIT_MS} ms waiting for ${what}`);
      await Bun.sleep(50);
    }
  }
  async function onTerminal(keys: string): Promise<string> {
    let shown = "";
    const decoder = new TextDecoder();
    const child = [
      `import { readRecipient } from ${JSON.stringify(join(import.meta.dir, "test-mail.ts"))};`,
      `try { process.stdout.write("RESULT " + (await readRecipient()) + "\\n"); } catch (error) { process.stdout.write("ERROR " + String(error) + "\\n"); }`,
    ].join("\n");
    const proc = Bun.spawn([process.execPath, "--eval", child], { terminal: { data: (_terminal, data) => void (shown += decoder.decode(data, { stream: true })) } });
    try {
      await waitFor(() => shown.includes(RECIPIENT_PROMPT.trim()), "the recipient prompt on the terminal");
      proc.terminal!.write(keys);
      await waitFor(() => /(RESULT|ERROR) [^\n]*\n/.test(shown), "the child's result line");
      await proc.exited;
      return shown;
    } finally {
      proc.kill();
      proc.terminal?.close();
    }
  }

  test.skipIf(process.platform === "win32")(
    "on a terminal: a prompt, the typing is not echoed, Ctrl-C cancels",
    async () => {
      const typed = await onTerminal(`${RECIPIENT}\r`);
      expect(typed).toContain(`RESULT ${RECIPIENT}`);
      expect(typed.split(RECIPIENT).length - 1).toBe(1);

      const cancelled = await onTerminal("\x03");
      expect(cancelled).toMatch(/ERROR [^\n]*cancelled: no recipient was read/);

      const bad = await onTerminal("a$(id)@example.com\r");
      expect(bad).toMatch(/ERROR [^\n]*recipient/);
      expect(bad).not.toContain("a$(id)");
    },
    4 * TERMINAL_WAIT_MS,
  );
});

describe("test-mail.ts: the pod", () => {
  const manifest = podManifest({ name: `${POD_PREFIX}run`, run: "run", smtp: SMTP, sender: SENDER });
  const spec = manifest.spec as { securityContext: Record<string, unknown>; containers: Record<string, unknown>[]; [key: string]: unknown };
  const container = spec.containers[0]!;

  test("satisfies Pod Security restricted, writes nothing and gets no token", () => {
    expect(spec.securityContext).toEqual({ runAsNonRoot: true, runAsUser: 100, runAsGroup: 101, seccompProfile: { type: "RuntimeDefault" } });
    expect(container.securityContext).toEqual({
      allowPrivilegeEscalation: false,
      privileged: false,
      readOnlyRootFilesystem: true,
      runAsNonRoot: true,
      runAsUser: 100,
      runAsGroup: 101,
      capabilities: { drop: ["ALL"] },
      seccompProfile: { type: "RuntimeDefault" },
    });
    expect(spec.automountServiceAccountToken).toBe(false);
    expect(spec.enableServiceLinks).toBe(false);
    expect(spec.restartPolicy).toBe("Never");
    expect(spec.activeDeadlineSeconds).toBeGreaterThan(0);
    for (const forbidden of ["volumes", "hostNetwork", "hostPID", "hostIPC", "serviceAccountName"]) expect(spec[forbidden]).toBeUndefined();
    expect(container.ports).toBeUndefined();
    expect((manifest.metadata as { namespace: string }).namespace).toBe(DEMO_NAMESPACE);
  });

  test("runs the curl image pinned by its index digest", () => {
    expect(IMAGE).toMatch(/^quay\.io\/curl\/curl:[0-9.]+@sha256:[0-9a-f]{64}$/);
    expect(container.image).toBe(IMAGE);
  });

  test("takes credentials and the sender only as Secret references, and the recipient only on stdin", () => {
    expect(container.env).toEqual([
      { name: "TESTMAIL_SMTP_USERNAME", valueFrom: { secretKeyRef: { name: "trustiot-smtp", key: "username" } } },
      { name: "TESTMAIL_SMTP_PASSWORD", valueFrom: { secretKeyRef: { name: "trustiot-smtp", key: "password" } } },
      { name: "TESTMAIL_FROM", valueFrom: { secretKeyRef: { name: "trustiot-demo-values", key: "smtp-from" } } },
    ]);
    expect({ stdin: container.stdin, stdinOnce: container.stdinOnce, tty: container.tty }).toEqual({ stdin: true, stdinOnce: true, tty: false });
    // Its arguments are the host, port, security, run ID, script and filter: no address, no secret.
    expect((container.command as string[]).slice(3, 8)).toEqual(["test-mail", "smtp.example.com", "587", "starttls", "run"]);
    expect(JSON.stringify(manifest)).not.toContain("@inbox");
  });

  test("the script gives curl no address or credential as an argument and requires TLS", () => {
    expect(POD_SCRIPT).toContain("--ssl-reqd");
    expect(POD_SCRIPT).toContain("--proto =smtp,smtps");
    expect(POD_SCRIPT).toContain("--expand-user '{{TESTMAIL_SMTP_USERNAME:trim}}:{{TESTMAIL_SMTP_PASSWORD:trim}}'");
    expect(POD_SCRIPT).toContain("--expand-mail-from '{{TESTMAIL_ENVELOPE_FROM}}'");
    expect(POD_SCRIPT).toContain("--expand-mail-rcpt '{{TESTMAIL_RCPT}}'");
    for (const option of ["--user ", "-u ", "--mail-from ", "--mail-rcpt ", "--insecure", " -k "]) expect(POD_SCRIPT).not.toContain(option);
    expect(POD_SCRIPT).toMatch(/starttls\) url="smtp:\/\/\$host:\$port"/);
    expect(POD_SCRIPT).toMatch(/tls\) url="smtps:\/\/\$host:\$port"/);
    // curl's transcript reaches nothing but the filter.
    expect(POD_SCRIPT).toContain(`2>&1 | awk "$filter"`);
  });

  test("run IDs make valid pod names", () => {
    const id = runId(new Date("2026-10-21T08:09:10.123Z"));
    expect(id).toMatch(/^20261021t080910-[0-9a-f]{4}$/);
    expect(`${POD_PREFIX}${id}`).toMatch(/^[a-z0-9]([-a-z0-9]{0,61}[a-z0-9])?$/);
  });
});

/** A curl -v transcript of a session, with what must never come out of the pod. */
const TRANSCRIPT = [
  "* Host smtp.example.com:587 was resolved.",
  "* IPv4: 192.0.2.10",
  "*   Trying 192.0.2.10:587...",
  "< 220 smtp.example.com ESMTP ready",
  "> EHLO trustiot-test-mail-run",
  "< 250-smtp.example.com",
  "< 250-STARTTLS",
  "< 250 AUTH PLAIN LOGIN",
  "> STARTTLS",
  "< 220 2.0.0 Ready to start TLS",
  "* SSL connection using TLSv1.3 / TLS_AES_256_GCM_SHA384 / X25519MLKEM768 / RSASSA-PSS",
  "* Server certificate:",
  "*   subject: CN=smtp.example.com",
  "* SSL certificate verified via OpenSSL.",
  "> EHLO trustiot-test-mail-run",
  "< 250 AUTH PLAIN LOGIN",
  "> AUTH PLAIN AHJlc2VuZAByZV9mYWtlX2tleQ==",
  "< 235 2.7.0 Authentication successful",
  "> MAIL FROM:<no-reply@mail.example.com> SIZE=420",
  "< 250 2.1.0 Ok no-reply@mail.example.com",
  "> RCPT TO:<someone@inbox.example.com>",
  "< 250 2.1.5 Ok someone@inbox.example.com",
  "> DATA",
  "< 354 End data with <CR><LF>.<CR><LF>",
  "* upload completely sent off: 420 bytes",
  "< 250 2.0.0 OK 0101019a-secret-message-id",
  "* Connection #0 to host smtp.example.com:587 left intact",
].join("\r\n");
const EXPECTED_LINES = [
  "TESTMAIL S 220",
  "TESTMAIL C EHLO",
  "TESTMAIL S 250",
  "TESTMAIL C STARTTLS",
  "TESTMAIL S 220 2.0.0",
  "TESTMAIL TLS TLSv1.3 TLS_AES_256_GCM_SHA384",
  "TESTMAIL TLS certificate-verified",
  "TESTMAIL C EHLO",
  "TESTMAIL S 250",
  "TESTMAIL C AUTH PLAIN",
  "TESTMAIL S 235 2.7.0",
  "TESTMAIL C MAIL FROM",
  "TESTMAIL S 250 2.1.0",
  "TESTMAIL C RCPT TO",
  "TESTMAIL S 250 2.1.5",
  "TESTMAIL C DATA",
  "TESTMAIL S 354",
  "TESTMAIL S 250 2.0.0",
];
const PRIVATE = ["someone", "inbox.example.com", "no-reply", "mail.example.com", "smtp.example.com", "192.0.2.10", "AHJlc2VuZAByZV9mYWtlX2tleQ", "secret-message-id", "trustiot-test-mail-run"];

describe("test-mail.ts: the pod's output", () => {
  test.skipIf(Bun.which("awk") === null)("the filter keeps reply codes, verbs and TLS only (this machine's awk; the image's BusyBox awk in the container test)", async () => {
    const out = await tryRun(["awk", OUTPUT_FILTER], { stdin: `${TRANSCRIPT}\r\ncurl: (67) Login denied for someone@inbox.example.com\r\n` });
    expect(out.exitCode).toBe(0);
    expect(out.stdout.trim().split("\n")).toEqual([...EXPECTED_LINES, "TESTMAIL CURL_ERROR 67"]);
    for (const text of PRIVATE) expect(out.stdout).not.toContain(text);
  });

  test("the workstation keeps only allowed lines and counts the rest", () => {
    const { lines, dropped } = filterOutput([...EXPECTED_LINES, "TESTMAIL EXIT 0", "sh: someone@inbox.example.com: not found", "TESTMAIL S 250 Ok someone@inbox.example.com", "TESTMAIL TLS a b c"].join("\n"));
    expect(lines).toEqual([...EXPECTED_LINES, "TESTMAIL EXIT 0"]);
    expect(dropped).toBe(3);
  });

  test("accepted only with verified TLS, a login, the envelope, DATA and the final 250", () => {
    const accepted = mailOutcome([...EXPECTED_LINES, "TESTMAIL EXIT 0"], "starttls");
    expect(accepted.accepted).toBe(true);
    expect(accepted.stages).toEqual([
      { step: "greeting", reply: "220" },
      { step: "EHLO", reply: "250" },
      { step: "STARTTLS", reply: "220 2.0.0" },
      { step: "EHLO", reply: "250" },
      { step: "AUTH PLAIN", reply: "235 2.7.0" },
      { step: "MAIL FROM", reply: "250 2.1.0" },
      { step: "RCPT TO", reply: "250 2.1.5" },
      { step: "DATA", reply: "354" },
      { step: "message", reply: "250 2.0.0" },
    ]);
    const without = (drop: string) => mailOutcome([...EXPECTED_LINES.filter((line) => line !== drop), "TESTMAIL EXIT 0"], "starttls").accepted;
    for (const line of ["TESTMAIL TLS certificate-verified", "TESTMAIL TLS TLSv1.3 TLS_AES_256_GCM_SHA384", "TESTMAIL S 235 2.7.0", "TESTMAIL C STARTTLS", "TESTMAIL S 354"]) expect(without(line)).toBe(false);
    expect(mailOutcome(EXPECTED_LINES, "starttls").accepted).toBe(false);
    expect(mailOutcome([...EXPECTED_LINES.slice(0, -1), "TESTMAIL S 554 5.7.1", "TESTMAIL EXIT 0"], "starttls").accepted).toBe(false);
    // Implicit TLS has no STARTTLS step.
    expect(mailOutcome([...EXPECTED_LINES.filter((l) => !l.includes("STARTTLS") && l !== "TESTMAIL S 220 2.0.0"), "TESTMAIL EXIT 0"], "tls").accepted).toBe(true);
  });

  test("report prints the stages and fails when the relay refused", () => {
    expect(() => report(log, { lines: ["TESTMAIL S 220", "TESTMAIL C EHLO", "TESTMAIL S 250", "TESTMAIL CURL_ERROR 67", "TESTMAIL EXIT 67"], dropped: 0, exitCode: 67 }, "starttls")).toThrow(/did not accept/);
    expect(logText()).toContain("curl error 67: the server refused the login");
    expect(report(log, { lines: [...EXPECTED_LINES, "TESTMAIL EXIT 0"], dropped: 1, exitCode: 0 }, "starttls").accepted).toBe(true);
    expect(logText()).toContain("message: 250 2.0.0");
  });
});

// --- The cluster, faked -------------------------------------------------------------------------

const CHECKOUT: Checkout = {
  head: "a".repeat(40),
  smtp: SMTP,
  sender: SENDER,
  layers: { saas: {}, demo: { host: "smtp.example.com", port: 587, security: "starttls", credentialsSecret: { name: "trustiot-smtp" } } },
};

interface Cluster {
  enforce: string;
  demoValues: string;
  smtpKeys: string[] | undefined;
  senderKeys: string[] | undefined;
  networkPolicies: Record<string, unknown>[];
  ztunnel: boolean;
  leftovers: string[];
}

const CLUSTER: Cluster = {
  enforce: "restricted",
  demoValues: "smtp:\n  host: smtp.example.com\n  port: 587\n  security: starttls\n  credentialsSecret:\n    name: trustiot-smtp\n",
  smtpKeys: ["password", "username"],
  senderKeys: ["app-host", "gateway-host", "ota-host", "smtp-from"],
  networkPolicies: [
    { metadata: { name: "trustiot-default-deny" }, spec: { podSelector: {}, policyTypes: ["Ingress"] } },
    { metadata: { name: "trustiot-identity" }, spec: { podSelector: { matchLabels: { "app.kubernetes.io/name": "identity" } }, policyTypes: ["Ingress"] } },
  ],
  ztunnel: true,
  leftovers: [],
};

/** Answers the plan's read-only kubectl calls; records every command. */
function clusterHandler(cluster: Cluster, commands: string[]): (command: string, stdin?: string) => RunResult | undefined {
  return (command) => {
    commands.push(command);
    if (command.includes("get namespace trustiot")) {
      return ok(JSON.stringify({ metadata: { name: "trustiot", labels: { "pod-security.kubernetes.io/enforce": cluster.enforce } } }));
    }
    if (command.includes("get configmap trustiot-values")) return ok(JSON.stringify({ data: { "values-saas.yaml": "global: {}\n", "values-demo.yaml": cluster.demoValues } }));
    if (command.includes("get secret trustiot-smtp")) return cluster.smtpKeys === undefined ? notFound('secrets "trustiot-smtp"') : ok(cluster.smtpKeys.map((k) => `${k}\n`).join(""));
    if (command.includes("get secret trustiot-demo-values")) return cluster.senderKeys === undefined ? notFound('secrets "trustiot-demo-values"') : ok(cluster.senderKeys.map((k) => `${k}\n`).join(""));
    if (command.includes("get networkpolicies")) return ok(JSON.stringify({ items: cluster.networkPolicies }));
    if (command.includes("get authorizationpolicies")) return ok(JSON.stringify({ items: [{ metadata: { name: "trustiot-identity" } }] }));
    if (command.includes("get daemonsets.apps")) return ok(JSON.stringify({ items: cluster.ztunnel ? [{ metadata: { namespace: "istio-system", name: "ztunnel" } }] : [] }));
    if (command.includes("get pods -l")) return ok(JSON.stringify({ items: cluster.leftovers.map((name) => ({ metadata: { name } })) }));
    return undefined;
  };
}

const planOf = (cluster: Partial<Cluster> = {}, commands: string[] = []) => {
  const handler = clusterHandler({ ...CLUSTER, ...cluster }, commands);
  return makePlan(
    fakeBox((command, stdin) => handler(command, stdin) ?? { stdout: "", stderr: `unexpected: ${command}`, exitCode: 1 }),
    log,
    { checkout: async () => CHECKOUT },
  );
};

describe("test-mail.ts: the plan", () => {
  test("binds the commit, the settings, the pod and what shapes its traffic; reads Secret key names only", async () => {
    const commands: string[] = [];
    const plan = await planOf({}, commands);
    expect(plan).toMatchObject({ head: CHECKOUT.head, namespace: "trustiot", image: IMAGE, smtp: SMTP, sender: SENDER, podSecurityEnforce: "restricted", ztunnel: ["istio-system/ztunnel"], leftovers: [] });
    expect(plan.networkPolicies).toEqual([
      { name: "trustiot-default-deny", types: ["Ingress"], selectsTestPod: "yes" },
      { name: "trustiot-identity", types: ["Ingress"], selectsTestPod: "no" },
    ]);
    expect(plan.manifestSha256).toMatch(/^[0-9a-f]{64}$/);
    const secretReads = commands.filter((c) => c.includes(" secret "));
    expect(secretReads.length).toBe(2);
    for (const read of secretReads) expect(read).toContain(`go-template=${SECRET_KEYS_TEMPLATE}`);
    expect(commands.every((c) => !/\b(create|apply|delete|patch|edit|label|annotate|scale|run|exec)\b/.test(c.replace(/^sudo k3s kubectl /, "")))).toBe(true);
    expect(logText()).toContain("no NetworkPolicy restricts egress");
  });

  test("the same cluster and checkout give the same plan; a change in what shapes the pod gives another", async () => {
    const { confirmCode } = await import("./lib.ts");
    const code = confirmCode(await planOf());
    expect(confirmCode(await planOf())).toBe(code);
    expect(confirmCode(await planOf({ networkPolicies: [...CLUSTER.networkPolicies, { metadata: { name: "egress" }, spec: { podSelector: {}, policyTypes: ["Egress"] } }] }))).not.toBe(code);
    expect(confirmCode(await planOf({ leftovers: [`${POD_PREFIX}old`] }))).not.toBe(code);
    expect(confirmCode(await planOf({ ztunnel: false }))).not.toBe(code);
  });

  test("reports egress rules that select the test pod", async () => {
    await planOf({ networkPolicies: [{ metadata: { name: "egress" }, spec: { podSelector: {}, egress: [] } }] });
    expect(logText()).toContain("egress is restricted by: egress (selects the test pod: yes)");
  });

  test("refuses a namespace without restricted, deployed values that differ, and missing Secrets or keys", async () => {
    await expect(planOf({ enforce: "baseline" })).rejects.toThrow(/not "restricted"/);
    await expect(planOf({ demoValues: CLUSTER.demoValues.replace("587", "2587") })).rejects.toThrow(/deployed values-demo.yaml has other smtp settings/);
    await expect(planOf({ smtpKeys: undefined })).rejects.toThrow(/Secret trustiot-smtp is not in trustiot/);
    await expect(planOf({ smtpKeys: ["username"] })).rejects.toThrow(/has no key password/);
    await expect(planOf({ senderKeys: ["app-host"] })).rejects.toThrow(/no key smtp-from/);
  });

  test("selectors: matchLabels must all match; matchExpressions make it uncertain", () => {
    expect(selects({}, POD_LABELS)).toBe("yes");
    expect(selects(undefined, POD_LABELS)).toBe("yes");
    expect(selects({ matchLabels: { "app.kubernetes.io/name": "trustiot-test-mail" } }, POD_LABELS)).toBe("yes");
    expect(selects({ matchLabels: { "app.kubernetes.io/name": "identity" } }, POD_LABELS)).toBe("no");
    expect(selects({ matchExpressions: [{ key: "a", operator: "Exists" }] }, POD_LABELS)).toBe("maybe");
    expect(networkPolicyFacts([{ metadata: { name: "x" }, spec: { egress: [{}] } }])).toEqual([{ name: "x", types: ["Egress", "Ingress"], selectsTestPod: "yes" }]);
  });
});

describe("test-mail.ts: the run", () => {
  const plan = async (leftovers: string[] = []): Promise<SendPlan> => ({ ...(await planOf()), leftovers });

  interface Seen {
    commands: string[];
    stdins: { command: string; stdin: string }[];
  }

  /** The cluster during a run: pod states in order, then the log. */
  function runBox(states: Record<string, unknown>[], logs: string, fail: Partial<Record<"create" | "attach", RunResult>> = {}): { box: Box; seen: Seen } {
    const seen: Seen = { commands: [], stdins: [] };
    let polls = 0;
    let deleted = false;
    const box = fakeBox((command, stdin) => {
      seen.commands.push(command);
      if (stdin !== undefined) seen.stdins.push({ command, stdin });
      if (command.includes(" create -f -")) return fail.create ?? ok("pod created");
      if (command.includes(" attach -i ")) return fail.attach ?? ok("");
      if (command.includes(" logs ")) return ok(logs);
      if (command.includes(" delete pod ")) {
        deleted = true;
        return ok("");
      }
      if (command.includes(" get pod ") && command.includes("-o name")) return ok(deleted ? "" : "pod/x\n");
      if (command.includes(" get pod ")) {
        const state = states[Math.min(polls, states.length - 1)]!;
        polls += 1;
        return ok(JSON.stringify(state));
      }
      return { stdout: "", stderr: `unexpected: ${command}`, exitCode: 1 };
    });
    return { box, seen };
  }

  const pending = { status: { phase: "Pending", containerStatuses: [{ name: "mail", state: { waiting: { reason: "ContainerCreating" } } }] } };
  const running = { status: { phase: "Running", containerStatuses: [{ name: "mail", state: { running: {} } }] } };
  const done = (exitCode: number) => ({ status: { phase: exitCode === 0 ? "Succeeded" : "Failed", containerStatuses: [{ name: "mail", state: { terminated: { exitCode, reason: exitCode === 0 ? "Completed" : "Error" } } }] } });
  const clock = () => {
    let now = 0;
    return { now: () => now, sleep: async (ms: number) => void (now += ms) };
  };

  test("the recipient goes only into the attach's stdin; the pod is created, read and deleted", async () => {
    const { box, seen } = runBox([pending, running, running, done(0)], [...EXPECTED_LINES, "TESTMAIL EXIT 0"].join("\n"));
    const result = await sendTestMail(box, log, await plan([`${POD_PREFIX}old`]), RECIPIENT, "20261021t000000-abcd", clock());
    expect(result).toEqual({ lines: [...EXPECTED_LINES, "TESTMAIL EXIT 0"], dropped: 0, exitCode: 0 });
    const kinds = seen.commands.map((c) => /kubectl -n trustiot (\w+)|kubectl (create)/.exec(c)?.slice(1).find(Boolean) ?? c);
    expect(kinds[0]).toBe("delete");
    expect(kinds).toContain("create");
    expect(kinds.indexOf("attach")).toBeGreaterThan(kinds.indexOf("create"));
    expect(kinds.lastIndexOf("delete")).toBeGreaterThan(kinds.indexOf("logs"));
    // Only the attach carries the recipient, and no command line or other input does.
    expect(seen.stdins.filter((s) => s.stdin.includes(RECIPIENT)).map((s) => s.command)).toEqual([seen.commands.find((c) => c.includes(" attach -i "))!]);
    expect(seen.stdins.find((s) => s.command.includes(" attach -i "))!.stdin).toBe(`${RECIPIENT}\n`);
    for (const command of seen.commands) expect(command).not.toContain("someone");
    const created = JSON.parse(seen.stdins.find((s) => s.command.includes(" create -f -"))!.stdin) as Record<string, unknown>;
    expect(created).toEqual(podManifest({ name: `${POD_PREFIX}20261021t000000-abcd`, run: "20261021t000000-abcd", smtp: SMTP, sender: SENDER }));
    expect(logText()).not.toContain("someone");
    expect(logText()).toContain(`deleted pod ${POD_PREFIX}20261021t000000-abcd`);
  });

  test("the attach is never repeated: the pod reads its stdin once", async () => {
    const { box, seen } = runBox([running, done(65)], "TESTMAIL REFUSED no-recipient\nTESTMAIL EXIT 65", { attach: { stdout: "", stderr: "connection lost", exitCode: 255 } });
    const result = await sendTestMail(box, log, await plan(), RECIPIENT, "r", clock());
    expect(seen.commands.filter((c) => c.includes(" attach -i ")).length).toBe(1);
    expect(result.exitCode).toBe(65);
  });

  test("the pod is deleted when it cannot start, when create fails and when it never ends", async () => {
    const stuck = { status: { phase: "Pending", containerStatuses: [{ name: "mail", state: { waiting: { reason: "CreateContainerConfigError" } } }] } };
    for (const [states, fail, error] of [
      [[stuck], {}, /cannot start: CreateContainerConfigError/],
      [[pending], {}, /did not run within/],
      [[running], {}, /did not end within/],
      [[running], { create: { stdout: "", stderr: 'pods "x" is forbidden: violates PodSecurity', exitCode: 1 } }, /kubectl create -f - failed/],
    ] as const) {
      const { box, seen } = runBox([...states], "", fail);
      await expect(sendTestMail(box, log, await plan(), RECIPIENT, "r", clock())).rejects.toThrow(error);
      expect(seen.commands.at(-2)).toContain(` delete pod ${POD_PREFIX}r `);
      expect(seen.stdins.filter((s) => s.stdin.includes(RECIPIENT)).length).toBeLessThanOrEqual(1);
    }
  });
});

// --- Received headers -----------------------------------------------------------------------------

const GMAIL = [
  "Delivered-To: someone@inbox.example.com",
  "Received: by 2002:a05:6000:1:b0:1:2:3 with SMTP id x1csp1;",
  "        Tue, 21 Oct 2026 08:00:01 -0700 (PDT)",
  "ARC-Authentication-Results: i=1; mx.inbox.example.com;",
  "       dkim=pass header.i=@mail.example.com header.s=resend header.b=AbCdEf12;",
  "       spf=pass (inbox.example.com: domain of bounces+1@send.mail.example.com designates 192.0.2.20 as permitted sender) smtp.mailfrom=bounces+1@send.mail.example.com;",
  "       dmarc=pass (p=NONE sp=NONE dis=NONE) header.from=mail.example.com",
  "Received: from a1-2.smtp-out.relay-provider.test (a1-2.smtp-out.relay-provider.test. [192.0.2.20])",
  "        by mx.inbox.example.com with ESMTPS id abc123",
  "        for <someone@inbox.example.com>",
  "        (version=TLS1_3 cipher=TLS_AES_128_GCM_SHA256 bits=128/128);",
  "        Tue, 21 Oct 2026 08:00:01 -0700 (PDT)",
  "Received-SPF: pass (inbox.example.com: domain of bounces+1@send.mail.example.com designates 192.0.2.20 as permitted sender) client-ip=192.0.2.20;",
  "Authentication-Results: mx.inbox.example.com;",
  "       dkim=pass header.i=@mail.example.com header.s=resend header.b=AbCdEf12;",
  "       dkim=pass header.i=@relay-provider.test header.s=key1 header.b=ZzYyXx;",
  "       spf=pass (inbox.example.com: domain of bounces+1@send.mail.example.com designates 192.0.2.20 as permitted sender) smtp.mailfrom=bounces+1@send.mail.example.com;",
  "       dmarc=pass (p=NONE sp=NONE dis=NONE) header.from=mail.example.com",
  "DKIM-Signature: v=1; a=rsa-sha256; c=relaxed/simple; s=resend; d=mail.example.com; t=1; h=From:To; bh=abc=; b=SIGNATURE",
  "DKIM-Signature: v=1; a=rsa-sha256; c=relaxed/simple; s=key1; d=relay-provider.test; t=1; h=From:To; bh=abc=; b=SIGNATURE2",
  "Message-ID: <0101019a-secret-message-id@relay-provider.test>",
  "From: TrustIoT Demo <no-reply@mail.example.com>",
  "To: someone@inbox.example.com",
  "Subject: TrustIoT demo SMTP test 20261021t000000-abcd",
  "",
  "A test message.",
].join("\r\n");

const HEADER_PRIVATE = ["someone", "inbox.example.com", "mail.example.com", "relay-provider.test", "no-reply", "bounces", "192.0.2.20", "AbCdEf12", "ZzYyXx", "SIGNATURE", "secret-message-id", "abc123", "2002:a05"];

describe("test-mail.ts: the received headers", () => {
  test("summarizes SPF, DKIM, DMARC, alignment and TLS without a domain, address or ID", () => {
    const { lines, pass } = headerSummary(GMAIL);
    expect(pass).toBe(true);
    expect(lines).toEqual([
      "From domain: <from-domain>",
      "Authentication-Results 1 (from the top) by <receiver-1>:",
      "  dkim=pass (d=<from-domain>, s=resend: aligned (strict and relaxed))",
      "  dkim=pass (d=<other-domain-1>, s=key1: not aligned)",
      "  spf=pass (MAIL FROM domain <subdomain of from-domain>: aligned (relaxed only))",
      "  dmarc=pass (header.from <from-domain>, p=NONE sp=NONE dis=NONE)",
      "ARC-Authentication-Results i=1 by <receiver-1>:",
      "  dkim=pass (d=<from-domain>, s=resend: aligned (strict and relaxed))",
      "  spf=pass (MAIL FROM domain <subdomain of from-domain>: aligned (relaxed only))",
      "  dmarc=pass (header.from <from-domain>, p=NONE sp=NONE dis=NONE)",
      "Received-SPF 1: pass",
      "DKIM-Signature 1: d=<from-domain>, a=rsa-sha256, s=resend",
      "DKIM-Signature 2: d=<other-domain-1>, a=rsa-sha256, s=key1",
      "Received 1 (from the top): with SMTP",
      "Received 2 (from the top): with ESMTPS, TLS1_3 TLS_AES_128_GCM_SHA256",
      "verdict: PASS (the first Authentication-Results has spf=pass and a dkim=pass aligned with From)",
    ]);
    const text = lines.join("\n");
    for (const value of HEADER_PRIVATE) expect(text).not.toContain(value);
  });

  test("fails when only an unaligned signature passes, or SPF does not", () => {
    const unaligned = GMAIL.replace("dkim=pass header.i=@mail.example.com header.s=resend header.b=AbCdEf12;\r\n       dkim=pass header.i=@relay", "dkim=fail header.i=@mail.example.com header.s=resend header.b=AbCdEf12;\r\n       dkim=pass header.i=@relay");
    expect(headerSummary(unaligned).pass).toBe(false);
    const softfail = GMAIL.replace(/(Authentication-Results: mx\.inbox\.example\.com;[\s\S]*?)spf=pass/, "$1spf=softfail");
    expect(headerSummary(softfail).pass).toBe(false);
    expect(headerSummary("From: a@mail.example.com\r\n\r\nbody").lines).toContain("Authentication-Results: none");
  });

  test("reads Microsoft's form and Postfix's TLS note", () => {
    const parsed = parseAuthResults("spf=pass (sender IP is 192.0.2.20) smtp.mailfrom=send.mail.example.com; dkim=pass (signature was verified) header.d=mail.example.com;dmarc=pass action=none header.from=mail.example.com;compauth=pass reason=100");
    expect(parsed.authserv).toBe("");
    expect(parsed.results.map((r) => `${r.method}=${r.result}`)).toEqual(["spf=pass", "dkim=pass", "dmarc=pass", "compauth=pass"]);
    expect(parsed.results[1]!.props["header.d"]).toBe("mail.example.com");
    const withId = parseAuthResults("mx.inbox.example.com 1; spf=pass smtp.mailfrom=send.mail.example.com; dkim=pass header.d=mail.example.com;compauth=pass reason=100");
    expect(withId.authserv).toBe("mx.inbox.example.com");
    expect(withId.results.map((r) => `${r.method}=${r.result}`)).toEqual(["spf=pass", "dkim=pass", "compauth=pass"]);
    expect(receivedTls("from x by y with Microsoft SMTP Server (version=TLS1_2, cipher=TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384) id 15.20")).toEqual({ protocol: undefined, version: "TLS1_2", cipher: "TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384" });
    expect(receivedTls("from x (x [192.0.2.1]) (using TLSv1.3 with cipher TLS_AES_256_GCM_SHA384 (256/256 bits)) by y with ESMTPS id Z")).toEqual({ protocol: "ESMTPS", version: "TLSv1.3", cipher: "TLS_AES_256_GCM_SHA384" });
  });

  test("relations to the From domain", () => {
    expect(relation("mail.example.com", "mail.example.com")).toBe("same");
    expect(relation("send.mail.example.com", "mail.example.com")).toBe("subdomain");
    expect(relation("example.com", "mail.example.com")).toBe("parent");
    expect(relation("other.example.com", "mail.example.com")).toBe("same-parent");
    expect(relation("relay-provider.test", "mail.example.com")).toBe("other");
  });
});

test("the lab lock is this issue's", () => {
  expect(ISSUE).toBe("Laysi/trustiot-v2#468");
});
