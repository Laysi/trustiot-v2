// A mail relay for the browser end-to-end test: it takes what the identity service sends (implicit
// TLS, no AUTH), keeps the messages in memory and lets the test read the sign-in code from them.
// It is the stand-in for the SMTP relay the cluster has (services/identity/README.md 「寄信（SMTP）」);
// nothing here leaves the host.
import { createServer, type TLSSocket } from "node:tls";

export interface Mail {
  from: string;
  to: string[];
  /** The decoded text body (the identity service sends UTF-8 as base64). */
  text: string;
}

export interface SmtpReceiver {
  port: number;
  mails(): readonly Mail[];
  close(): Promise<void>;
}

/** What a client may send without a line break before the relay drops it. */
const MAX_PENDING = 64 * 1024;

/** The sign-in code in a message: the first run of exactly 8 digits. */
export function codeIn(text: string): string | undefined {
  return /(?<![0-9])[0-9]{8}(?![0-9])/.exec(text)?.[0];
}

/** The text of a message: headers end at the first blank line, the body is base64 or plain. */
export function decodeMessage(data: string): string {
  const split = data.indexOf("\r\n\r\n");
  const head = split < 0 ? data : data.slice(0, split);
  const body = split < 0 ? "" : data.slice(split + 4);
  if (/^content-transfer-encoding:\s*base64\s*$/im.test(head)) {
    return Buffer.from(body.replaceAll("\r\n", ""), "base64").toString("utf8");
  }
  return body;
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
