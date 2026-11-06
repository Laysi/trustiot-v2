// A mail relay for the browser end-to-end test: it takes what the identity service sends (implicit
// TLS, no AUTH), keeps the messages in memory and lets the test read the sign-in code from them.
// It is the stand-in for the SMTP relay the cluster has (services/identity/README.md 「寄信（SMTP）」);
// nothing here leaves the host.
import { createServer, type TLSSocket } from "node:tls";

export interface Mail {
  from: string;
  to: string[];
  /**
   * The decoded plain text: the identity service sends `multipart/alternative` with a `text/plain`
   * and a `text/html` part, both UTF-8 as base64; this is the `text/plain` one.
   */
  text: string;
}

export interface SmtpReceiver {
  port: number;
  mails(): readonly Mail[];
  close(): Promise<void>;
}

/** What a client may send without a line break before the relay drops it. */
const MAX_PENDING = 64 * 1024;

/** Digits in a sign-in code (`CODE_DIGITS` in services/identity/src/login.rs). */
export const CODE_DIGITS = 6;

/** The sign-in code in a message: the first run of exactly {@link CODE_DIGITS} digits. */
export function codeIn(text: string): string | undefined {
  return new RegExp(`(?<![0-9])[0-9]{${CODE_DIGITS}}(?![0-9])`).exec(text)?.[0];
}

/** The value of the first header called `name` (any case), unfolded. */
function header(head: string, name: string): string | undefined {
  const unfolded = head.replaceAll(/\r\n[ \t]+/g, " ");
  const prefix = `${name.toLowerCase()}:`;
  const line = unfolded.split("\r\n").find((candidate) => candidate.toLowerCase().startsWith(prefix));
  return line?.slice(prefix.length).trim();
}

/**
 * The text of a message or of one MIME part: headers end at the first blank line, the body is
 * base64 or plain. Of a `multipart/*` body it is the text of the first `text/plain` part, or of the
 * first part when none is `text/plain`.
 */
export function decodeMessage(data: string): string {
  // A part without headers starts with the blank line.
  const split = data.startsWith("\r\n") ? 0 : data.indexOf("\r\n\r\n");
  const head = split < 0 ? data : data.slice(0, split);
  const body = split < 0 ? "" : data.slice(split === 0 ? 2 : split + 4);
  const contentType = header(head, "content-type") ?? "";
  if (/^multipart\//i.test(contentType)) {
    const boundary = /;\s*boundary="?([^";]+)"?/i.exec(contentType)?.[1];
    if (boundary === undefined) return body;
    const parts = partsOf(body, boundary);
    const plain = parts.find((part) => /^text\/plain\b/i.test(header(partHead(part), "content-type") ?? "text/plain"));
    const chosen = plain ?? parts[0];
    return chosen === undefined ? "" : decodeMessage(chosen);
  }
  if (/^base64$/i.test(header(head, "content-transfer-encoding") ?? "")) {
    return Buffer.from(body.replaceAll("\r\n", ""), "base64").toString("utf8");
  }
  return body;
}

/** The headers of a part (a part without headers starts with the blank line). */
function partHead(part: string): string {
  if (part.startsWith("\r\n")) return "";
  const split = part.indexOf("\r\n\r\n");
  return split < 0 ? part : part.slice(0, split);
}

/** The parts of a multipart body (RFC 2046, 5.1.1): between `--boundary` lines, up to `--boundary--`. */
function partsOf(body: string, boundary: string): string[] {
  const parts: string[] = [];
  let current: string[] | null = null;
  for (const line of body.split("\r\n")) {
    if (line === `--${boundary}--`) break;
    if (line === `--${boundary}`) {
      if (current !== null) parts.push(current.join("\r\n"));
      current = [];
    } else if (current !== null) {
      current.push(line);
    }
  }
  if (current !== null) parts.push(current.join("\r\n"));
  return parts;
}

/** Undoes the dot-stuffing of the DATA phase (RFC 5321, 4.5.2). */
function unstuff(lines: string[]): string {
  return lines.map((line) => (line.startsWith("..") ? line.slice(1) : line)).join("\r\n");
}

function address(argument: string): string {
  const match = /<([^>]*)>/.exec(argument);
  return (match?.[1] ?? argument).trim();
}

function serve(socket: TLSSocket, store: Mail[]): void {
  let buffer = "";
  let from = "";
  let to: string[] = [];
  let data: string[] | null = null;
  const reply = (text: string) => socket.write(`${text}\r\n`);
  const handle = (line: string) => {
    if (data !== null) {
      if (line === ".") {
        const message = unstuff(data);
        data = null;
        store.push({ from, to, text: decodeMessage(message) });
        from = "";
        to = [];
        reply("250 2.0.0 queued");
      } else {
        data.push(line);
      }
      return;
    }
    const [verb = "", ...rest] = line.split(" ");
    const argument = rest.join(" ");
    switch (verb.toUpperCase()) {
      case "EHLO":
        reply("250-localhost");
        reply("250 8BITMIME");
        break;
      case "HELO":
        reply("250 localhost");
        break;
      case "MAIL":
        from = address(argument);
        to = [];
        reply("250 2.1.0 ok");
        break;
      case "RCPT":
        to.push(address(argument));
        reply("250 2.1.5 ok");
        break;
      case "DATA":
        data = [];
        reply("354 end with <CRLF>.<CRLF>");
        break;
      case "RSET":
        from = "";
        to = [];
        reply("250 2.0.0 ok");
        break;
      case "NOOP":
        reply("250 2.0.0 ok");
        break;
      case "QUIT":
        socket.end("221 2.0.0 bye\r\n");
        break;
      default:
        reply("502 5.5.2 command not implemented");
    }
  };
  socket.setEncoding("utf8");
  socket.on("data", (chunk: string) => {
    buffer += chunk;
    for (let end = buffer.indexOf("\r\n"); end >= 0; end = buffer.indexOf("\r\n")) {
      const line = buffer.slice(0, end);
      buffer = buffer.slice(end + 2);
      handle(line);
    }
    if (buffer.length > MAX_PENDING) socket.destroy();
  });
  // A client that goes away in the middle of a message is not the test's problem.
  socket.on("error", () => socket.destroy());
  reply("220 localhost ESMTP ready");
}

/** Starts the relay on 127.0.0.1 (`port` 0 picks a free one) with TLS from `key` and `certificate`. */
export function startSmtpReceiver(options: { key: string; certificate: string; port?: number }): Promise<SmtpReceiver> {
  const store: Mail[] = [];
  const sockets = new Set<TLSSocket>();
  const server = createServer({ key: options.key, cert: options.certificate }, (socket) => {
    sockets.add(socket);
    socket.once("close", () => sockets.delete(socket));
    serve(socket, store);
  });
  return new Promise((resolve, reject) => {
    server.once("error", reject);
    server.listen({ host: "127.0.0.1", port: options.port ?? 0 }, () => {
      const { port } = server.address() as { port: number };
      resolve({
        port,
        mails: () => store,
        close: () =>
          new Promise<void>((done) => {
            server.close(() => done());
            for (const socket of sockets) socket.destroy();
          }),
      });
    });
  });
}
