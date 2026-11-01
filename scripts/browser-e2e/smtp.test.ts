// The mail relay of the browser end-to-end run: it speaks enough SMTP over TLS for the identity
// service's client, and hands the test the code in the message.
import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { connect, type TLSSocket } from "node:tls";

import { generateDevCertificates } from "../lib/tls.ts";
import { codeIn, decodeMessage, type SmtpReceiver, startSmtpReceiver } from "./smtp.ts";

const certificates = generateDevCertificates("smtp-unit");
let receiver: SmtpReceiver;

beforeAll(async () => {
  receiver = await startSmtpReceiver({ key: certificates.serverKey, certificate: certificates.serverCertificate });
});
afterAll(async () => {
  await receiver.close();
});

/** A line of the message that follows DATA: the relay says nothing until the terminating dot. */
const text = (line: string): Line => ({ line, reply: false });
interface Line {
  line: string;
  reply: boolean;
}

/** Sends each line after the previous reply, as a client does, and returns every reply line. */
async function converse(commands: readonly (string | Line)[]): Promise<string[]> {
  const socket: TLSSocket = connect({ host: "127.0.0.1", port: receiver.port, ca: certificates.caCertificate });
  socket.setEncoding("utf8");
  const replies: string[] = [];
  let pending = "";
  let waiting: (() => void) | null = null;
  socket.on("data", (chunk: string) => {
    pending += chunk;
    waiting?.();
  });
  const reply = async (): Promise<void> => {
    // A final reply line has a space after the code; a continuation has a hyphen.
    for (;;) {
      const end = pending.indexOf("\r\n");
      if (end >= 0) {
        const line = pending.slice(0, end);
        pending = pending.slice(end + 2);
        replies.push(line);
        if (line[3] === " ") return;
        continue;
      }
      await new Promise<void>((resolve) => {
        waiting = resolve;
      });
    }
  };
  await reply();
  for (const command of commands) {
    const { line, reply: expectsReply } = typeof command === "string" ? { line: command, reply: true } : command;
    socket.write(`${line}\r\n`);
    if (expectsReply) await reply();
  }
  socket.destroy();
  return replies;
}

describe("the relay", () => {
  test("takes a message, decodes the base64 body and finds the code", async () => {
    const body = Buffer.from("登入驗證碼 Your code is 12345678\r\n", "utf8").toString("base64");
    const replies = await converse([
      "EHLO test",
      "MAIL FROM:<noreply@example.com>",
      "RCPT TO:<alice@example.com>",
      "DATA",
      text("Content-Transfer-Encoding: base64"),
      text(""),
      text(body),
      ".",
      "QUIT",
    ]);
    expect(replies.map((line) => line.slice(0, 3))).toEqual(["220", "250", "250", "250", "250", "354", "250", "221"]);
    const mail = receiver.mails().at(-1);
    expect(mail?.from).toBe("noreply@example.com");
    expect(mail?.to).toEqual(["alice@example.com"]);
    expect(mail?.text).toContain("Your code is 12345678");
    expect(codeIn(mail?.text ?? "")).toBe("12345678");
  });

  test("refuses a command it does not know, without dropping the connection", async () => {
    const replies = await converse(["EHLO test", "VRFY alice", "QUIT"]);
    expect(replies.map((line) => line.slice(0, 3))).toEqual(["220", "250", "250", "502", "221"]);
  });
});

describe("codeIn", () => {
  test("is the first run of exactly eight digits", () => {
    expect(codeIn("code 87654321 and 12345678")).toBe("87654321");
    expect(codeIn("a 123456789 run of nine and 1234567 of seven")).toBeUndefined();
    expect(codeIn("")).toBeUndefined();
  });
});

describe("decodeMessage", () => {
  test("leaves a plain body as it is and decodes a base64 one", () => {
    expect(decodeMessage("Subject: x\r\n\r\nplain 00000000")).toBe("plain 00000000");
    expect(decodeMessage("Content-Transfer-Encoding: base64\r\n\r\naGk=")).toBe("hi");
  });
});
