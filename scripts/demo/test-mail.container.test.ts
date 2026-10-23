// The test pod's script and filter in the pinned curl image, against a fake SMTP relay on this
// workstation (Laysi/trustiot-v2#468). It needs podman and pulls the image (its linux/amd64 or
// linux/arm64 variant, whichever this machine runs; both come from the same index), so it runs only
// with TRUSTIOT_DEMO_MAIL_E2E=1:
//
//   TRUSTIOT_DEMO_MAIL_E2E=1 bun test scripts/demo/test-mail.container.test.ts
//
// The container runs as the pod does: the image's UID and GID, every capability dropped, no new
// privileges, a read-only root file system with no writable tmpfs on /tmp, /var/tmp or /run
// (--read-only-tmpfs=false, as readOnlyRootFilesystem gives none), the credentials and the sender
// as env variables (passed by name, so their values are not podman arguments either) and the
// recipient on stdin. Each container has its own name and is removed with its volumes in every
// case, a killed podman run included.
//
// Where it differs from the pod: podman runs it instead of the kubelet (podman's default seccomp
// profile rather than RuntimeDefault, no Pod Security admission); it trusts the relay's throwaway
// CA through CURL_CA_BUNDLE instead of the image's own CA bundle; and its network is its own
// namespace in which pasta forwards only the relay's port on the IPv4 loopback to the relay on the
// host's IPv4 loopback (no host networking, nothing published; IPv4 only, so curl's try of ::1 is
// refused at once instead of being forwarded to a host address nothing serves), not the pod
// network and ztunnel.
import { afterAll, beforeAll, describe, expect, setDefaultTimeout, test } from "bun:test";
import { randomUUID } from "node:crypto";
import { chmodSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { createServer, type Server, type Socket } from "node:net";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { TLSSocket, createSecureContext, type SecureContext } from "node:tls";
import { generateDevCertificates } from "../lib/tls.ts";
import { ROOT } from "./lib.ts";
import { IMAGE, IMAGE_GID, IMAGE_UID, filterOutput, mailOutcome, podCommand, type Security, type SmtpSettings } from "./test-mail.ts";

const ENABLED = process.env.TRUSTIOT_DEMO_MAIL_E2E === "1";
// Pulling the image and starting containers on a loaded machine; each podman run is bounded by
// the pod's own curl limits (20 s connect, 90 s session) and the stdin wait (120 s).
const PODMAN_RUN_MS = 240_000;
setDefaultTimeout(PODMAN_RUN_MS + 60_000);

const USERNAME = "resend";
// A fresh value on every run, which the output must never contain.
const PASSWORD = `re_${randomUUID()}`;
/** The chart's default key names in the SMTP Secret (deploy/chart/values.yaml, smtp.credentialsSecret). */
const KEYS = (Bun.YAML.parse(readFileSync(join(ROOT, "deploy/chart/values.yaml"), "utf8")) as { smtp: { credentialsSecret: { usernameKey: string; passwordKey: string } } }).smtp.credentialsSecret;
const credentials = (name: string) => ({ name, usernameKey: KEYS.usernameKey, passwordKey: KEYS.passwordKey });
const SENDER_ADDRESS = "no-reply@mail.example.com";
const SENDER = `TrustIoT Demo <${SENDER_ADDRESS}>`;
const RECIPIENT = "someone@inbox.example.com";
/** What the fake relay puts into its reply to the message: it must never reach the output. */
const QUEUE_ID = "queued-as-0f1e2d3c4b5a";

interface Session {
  commands: string[];
  auth: string[];
  message: string;
  tls: boolean;
}

interface RelayOptions {
  /** starttls: plain greeting, STARTTLS offered; tls: TLS from the first byte. */
  mode: Security;
  /** Leave STARTTLS out of the EHLO reply (a relay that cannot encrypt). */
  noStarttls?: boolean;
}

/** A fake SMTP relay: just enough of RFC 5321, 3207 and 4954 for curl. */
class Relay {
  readonly sessions: Session[] = [];
  private server: Server | undefined;
  port = 0;

  constructor(
    private readonly context: SecureContext,
    private readonly options: RelayOptions,
  ) {}

  async start(): Promise<void> {
    this.server = createServer((raw) => this.serve(raw));
    await new Promise<void>((resolve) => this.server!.listen(0, "127.0.0.1", resolve));
    this.port = (this.server.address() as { port: number }).port;
  }

  stop(): void {
    this.server?.close();
  }

  private serve(raw: Socket): void {
    const session: Session = { commands: [], auth: [], message: "", tls: this.options.mode === "tls" };
    this.sessions.push(session);
    let stream: Socket = this.options.mode === "tls" ? new TLSSocket(raw, { isServer: true, secureContext: this.context }) : raw;
    let buffer = "";
    let inData = false;
    let authStep: "none" | "plain" = "none";
    const reply = (text: string) => stream.write(`${text}\r\n`);
    const onLine = (line: string): void => {
      if (inData) {
        if (line === ".") {
          inData = false;
          reply(`250 2.0.0 Ok: ${QUEUE_ID} for ${RECIPIENT}`);
        } else session.message += `${line}\n`;
        return;
      }
      if (authStep === "plain") {
        authStep = "none";
        session.auth.push(Buffer.from(line, "base64").toString("utf8"));
        reply(session.auth.at(-1) === `\0${USERNAME}\0${PASSWORD}` ? "235 2.7.0 Authentication successful" : "535 5.7.8 Authentication failed");
        return;
      }
      session.commands.push(line);
      const upper = line.toUpperCase();
      if (upper.startsWith("EHLO")) {
        const lines = ["fake-relay.test", "AUTH PLAIN", ...(session.tls || this.options.noStarttls ? [] : ["STARTTLS"]), "SIZE 1000000"];
        lines.forEach((text, index) => reply(`250${index === lines.length - 1 ? " " : "-"}${text}`));
      } else if (upper === "STARTTLS" && !session.tls && !this.options.noStarttls) {
        reply("220 2.0.0 Ready to start TLS");
        stream.removeAllListeners("data");
        const secure = new TLSSocket(raw, { isServer: true, secureContext: this.context });
        session.tls = true;
        stream = secure;
        buffer = "";
        secure.on("data", onData);
        secure.on("error", () => {});
      } else if (upper.startsWith("AUTH PLAIN")) {
        const initial = line.split(" ")[2];
        if (initial === undefined) {
          authStep = "plain";
          reply("334 ");
        } else {
          authStep = "plain";
          onLine(initial);
        }
      } else if (upper.startsWith("MAIL FROM:")) reply(`250 2.1.0 Ok ${SENDER_ADDRESS}`);
      else if (upper.startsWith("RCPT TO:")) reply(`250 2.1.5 Ok ${RECIPIENT}`);
      else if (upper === "DATA") {
        inData = true;
        reply("354 End data with <CR><LF>.<CR><LF>");
      } else if (upper === "QUIT") {
        reply("221 2.0.0 Bye");
        stream.end();
      } else reply("502 5.5.2 Command not recognized");
    };
    const onData = (chunk: Buffer): void => {
      buffer += chunk.toString("utf8");
      for (let index = buffer.indexOf("\r\n"); index >= 0; index = buffer.indexOf("\r\n")) {
        const line = buffer.slice(0, index);
        buffer = buffer.slice(index + 2);
        onLine(line);
      }
    };
    stream.on("data", onData);
    stream.on("error", () => {});
    if (this.options.mode === "starttls") reply("220 fake-relay.test ESMTP");
    else (stream as TLSSocket).once("secure", () => reply("220 fake-relay.test ESMTP"));
  }
}

let dir = "";
let context: SecureContext;

beforeAll(() => {
  if (!ENABLED) return;
  const certificates = generateDevCertificates("test-mail");
  dir = mkdtempSync(join(tmpdir(), "test-mail-ca-"));
  // The container's UID maps to another user on the host: it must be able to enter the directory.
  // It holds only the throwaway CA's public certificate.
  chmodSync(dir, 0o755);
  writeFileSync(join(dir, "ca.pem"), certificates.caCertificate, { mode: 0o644 });
  context = createSecureContext({ cert: certificates.serverCertificate, key: certificates.serverKey });
});

afterAll(() => {
  if (dir !== "") rmSync(dir, { recursive: true, force: true });
});

const smtp = (port: number, security: Security): SmtpSettings => ({
  host: "localhost",
  port,
  security,
  credentialsSecret: credentials("trustiot-smtp"),
});

/** Runs the pod's command in the image, as the pod does, and returns its stdout and exit status. */
async function runPod(settings: SmtpSettings, stdin: string, env: Record<string, string> = {}): Promise<{ stdout: string; stderr: string; exitCode: number }> {
  const [entrypoint, ...args] = podCommand(settings, "20261021t000000-abcd");
  const values = { TESTMAIL_SMTP_USERNAME: USERNAME, TESTMAIL_SMTP_PASSWORD: PASSWORD, TESTMAIL_FROM: SENDER, CURL_CA_BUNDLE: "/ca/ca.pem", ...env };
  const name = `trustiot-test-mail-${randomUUID()}`;
  const proc = Bun.spawn(
    [
      "podman", "run", "--rm", "--name", name, "-i", "--network", `pasta:-4,-T,${settings.port}`,
      "--read-only", "--read-only-tmpfs=false", "--cap-drop", "ALL", "--security-opt", "no-new-privileges",
      "--user", `${IMAGE_UID}:${IMAGE_GID}`, "-v", `${dir}:/ca:ro,Z`,
      ...Object.keys(values).flatMap((key) => ["-e", key]),
      "--entrypoint", entrypoint!, IMAGE, ...args,
    ],
    { stdin: new Blob([stdin]), stdout: "pipe", stderr: "pipe", env: { ...process.env, ...values } },
  );
  const timer = setTimeout(() => proc.kill(), PODMAN_RUN_MS);
  try {
    const [stdout, stderr, exitCode] = await Promise.all([new Response(proc.stdout).text(), new Response(proc.stderr).text(), proc.exited]);
    return { stdout, stderr, exitCode };
  } finally {
    clearTimeout(timer);
    // --rm has usually removed it already; this covers a killed podman run. Never leave volumes.
    const removed = Bun.spawn(["podman", "rm", "--force", "--volumes", "--time", "0", name], { stdout: "pipe", stderr: "pipe" });
    const [removeErr, removeExit] = await Promise.all([new Response(removed.stderr).text(), removed.exited]);
    if (removeExit !== 0 && !/no such container/i.test(removeErr)) throw new Error(`podman rm --force --volumes ${name} failed: ${removeErr.trim()}`);
  }
}

/** Nothing private in the output: no address, domain, credential or queue ID. */
function expectClean(stdout: string): void {
  for (const secret of [RECIPIENT, "inbox.example.com", SENDER_ADDRESS, "mail.example.com", PASSWORD, Buffer.from(`\0${USERNAME}\0${PASSWORD}`).toString("base64"), QUEUE_ID, "localhost", "127.0.0.1"]) {
    expect(stdout).not.toContain(secret);
  }
  expect(filterOutput(stdout).dropped).toBe(0);
}

describe.skipIf(!ENABLED)("the test pod's script in the pinned image", () => {
  for (const security of ["starttls", "tls"] as const) {
    test(`sends with ${security}: verified TLS, login, envelope and message, and prints only codes`, async () => {
      const relay = new Relay(context, { mode: security });
      await relay.start();
      try {
        const out = await runPod(smtp(relay.port, security), `${RECIPIENT}\n`);
        expectClean(out.stdout);
        const outcome = mailOutcome(filterOutput(out.stdout).lines, security, out.exitCode);
        expect({ accepted: outcome.accepted, exit: out.exitCode, stderr: out.stderr }).toEqual({ accepted: true, exit: 0, stderr: "" });
        expect(outcome.tls).toMatch(/^TLSv1\.[23] [A-Z0-9_-]+$/);
        expect(outcome.stages.map((s) => s.step)).toEqual([
          "greeting",
          "EHLO",
          ...(security === "starttls" ? ["STARTTLS", "EHLO"] : []),
          "AUTH PLAIN",
          "AUTH (continued)",
          "MAIL FROM",
          "RCPT TO",
          "DATA",
          // curl ends the session without QUIT.
          "message",
        ]);
        const session = relay.sessions[0]!;
        expect(session.tls).toBe(true);
        expect(session.auth).toEqual([`\0${USERNAME}\0${PASSWORD}`]);
        expect(session.commands.filter((c) => /^(MAIL|RCPT)/.test(c)).map((c) => c.replace(/ SIZE=\d+$/, ""))).toEqual([`MAIL FROM:<${SENDER_ADDRESS}>`, `RCPT TO:<${RECIPIENT}>`]);
        expect(session.message).toContain(`From: ${SENDER}\n`);
        expect(session.message).toContain(`To: ${RECIPIENT}\n`);
        expect(session.message).toMatch(/^Subject: TrustIoT demo SMTP test 20261021t000000-abcd$/m);
      } finally {
        relay.stop();
      }
    });
  }

  test("a refused login is curl error 67, not accepted", async () => {
    const relay = new Relay(context, { mode: "starttls" });
    await relay.start();
    try {
      const out = await runPod(smtp(relay.port, "starttls"), `${RECIPIENT}\n`, { TESTMAIL_SMTP_PASSWORD: "not-the-password" });
      expectClean(out.stdout);
      expect(out.stdout).not.toContain("not-the-password");
      const outcome = mailOutcome(filterOutput(out.stdout).lines, "starttls", out.exitCode);
      expect({ accepted: outcome.accepted, curlError: outcome.curlError, exit: out.exitCode }).toEqual({ accepted: false, curlError: 67, exit: 67 });
      expect(relay.sessions[0]!.commands.some((c) => c.startsWith("MAIL"))).toBe(false);
    } finally {
      relay.stop();
    }
  });

  test("a relay without STARTTLS gets no login and no mail", async () => {
    const relay = new Relay(context, { mode: "starttls", noStarttls: true });
    await relay.start();
    try {
      const out = await runPod(smtp(relay.port, "starttls"), `${RECIPIENT}\n`);
      expectClean(out.stdout);
      const outcome = mailOutcome(filterOutput(out.stdout).lines, "starttls", out.exitCode);
      expect({ accepted: outcome.accepted, curlError: outcome.curlError }).toEqual({ accepted: false, curlError: 64 });
      expect(relay.sessions[0]!.auth).toEqual([]);
      expect(relay.sessions[0]!.commands.some((c) => /^(AUTH|MAIL)/.test(c))).toBe(false);
    } finally {
      relay.stop();
    }
  });

  test("a recipient the shell would interpret, or none, is refused before curl runs", async () => {
    const relay = new Relay(context, { mode: "starttls" });
    await relay.start();
    try {
      for (const [stdin, refusal] of [
        ["a$(id)@inbox.example.com\n", "recipient"],
        ["someone@inbox.example.com>,<other@example.com\n", "recipient"],
        ["", "no-recipient"],
      ] as const) {
        const out = await runPod(smtp(relay.port, "starttls"), stdin);
        expect(filterOutput(out.stdout).lines).toEqual([`TESTMAIL REFUSED ${refusal}`]);
        expect(out.exitCode).toBe(65);
      }
      const sender = await runPod(smtp(relay.port, "starttls"), `${RECIPIENT}\n`, { TESTMAIL_FROM: "Demo <a@mail.example.com>\nBcc: x@example.com" });
      expect(filterOutput(sender.stdout).lines).toEqual(["TESTMAIL REFUSED sender"]);
      expect(relay.sessions).toEqual([]);
    } finally {
      relay.stop();
    }
  });
});
