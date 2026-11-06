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

const base64 = (content: string) => Buffer.from(content, "utf8").toString("base64");

/**
 * A message shaped like the identity service's (services/identity/src/mail.rs): multipart/alternative,
 * plain text first, HTML last, both base64. The HTML holds another number first, so reading the
 * wrong part finds the wrong code.
 */
function identityMessage(plain: string, html: string): string[] {
  const boundary = "=_trustiot_0123456789abcdef0123456789abcdef";
  return [
    "Subject: =?UTF-8?B?VHJ1c3RJb1Qg?=",
    "MIME-Version: 1.0",
    `Content-Type: multipart/alternative; boundary="${boundary}"`,
    "",
    `--${boundary}`,
    "Content-Type: text/plain; charset=utf-8",
    "Content-Transfer-Encoding: base64",
    "",
    base64(plain),
    `--${boundary}`,
    "Content-Type: text/html; charset=utf-8",
    "Content-Transfer-Encoding: base64",
    "",
    base64(html),
    `--${boundary}--`,
  ];
}

describe("the relay", () => {
  test("takes a multipart message, decodes its text/plain part and finds the code", async () => {
    const lines = identityMessage(
      "482913 是您的 TrustIoT 登入驗證碼 · Your TrustIoT login code is 482913\r\n",
      '<p style="color:#111111">999999</p><span>482913</span>',
    );
    const replies = await converse([
      "EHLO test",
      "MAIL FROM:<noreply@example.com>",
      "RCPT TO:<alice@example.com>",
      "DATA",
      ...lines.map(text),
      ".",
      "QUIT",
    ]);
    expect(replies.map((line) => line.slice(0, 3))).toEqual(["220", "250", "250", "250", "250", "354", "250", "221"]);
    const mail = receiver.mails().at(-1);
    expect(mail?.from).toBe("noreply@example.com");
    expect(mail?.to).toEqual(["alice@example.com"]);
    expect(mail?.text).toStartWith("482913 是您的 TrustIoT 登入驗證碼");
    expect(mail?.text).not.toContain("<p");
    expect(codeIn(mail?.text ?? "")).toBe("482913");
  });

  test("refuses a command it does not know, without dropping the connection", async () => {
    const replies = await converse(["EHLO test", "VRFY alice", "QUIT"]);
    expect(replies.map((line) => line.slice(0, 3))).toEqual(["220", "250", "250", "502", "221"]);
  });
});

describe("codeIn", () => {
  test("is the first run of exactly six digits", () => {
    expect(codeIn("code 654321 and 123456")).toBe("654321");
    expect(codeIn("a 12345678 run of eight, 1234567 of seven and 12345 of five")).toBeUndefined();
    expect(codeIn("")).toBeUndefined();
  });
});

describe("decodeMessage", () => {
  test("leaves a plain body as it is and decodes a base64 one", () => {
    expect(decodeMessage("Subject: x\r\n\r\nplain 000000")).toBe("plain 000000");
    expect(decodeMessage("Content-Transfer-Encoding: base64\r\n\r\naGk=")).toBe("hi");
  });

  test("reads the text/plain part of a multipart message, whatever the order of the parts", () => {
    const message = identityMessage("plain 111111", "<p>222222</p>").join("\r\n");
    expect(decodeMessage(message)).toBe("plain 111111");
    const boundary = "b";
    const htmlFirst = [
      `Content-Type: multipart/alternative;\r\n boundary=${boundary}`,
      "",
      "preamble",
      `--${boundary}`,
      "Content-Type: text/html; charset=utf-8",
      "",
      "<p>333333</p>",
      `--${boundary}`,
      "Content-Type: text/plain; charset=utf-8",
      "Content-Transfer-Encoding: base64",
      "",
      base64("plain 444444"),
      `--${boundary}--`,
      "epilogue",
    ].join("\r\n");
    expect(decodeMessage(htmlFirst)).toBe("plain 444444");
  });

  test("takes the first part when none is text/plain, and a part without headers as plain text", () => {
    const htmlOnly = ['Content-Type: multipart/alternative; boundary="x"', "", "--x", "Content-Type: text/html", "", "<p>555555</p>", "--x--"].join(
      "\r\n",
    );
    expect(decodeMessage(htmlOnly)).toBe("<p>555555</p>");
    const bare = ['Content-Type: multipart/alternative; boundary="x"', "", "--x", "", "plain 666666", "--x--"].join("\r\n");
    expect(decodeMessage(bare)).toBe("plain 666666");
  });
});
