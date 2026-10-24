// Measures when Mosquitto (the chart's pinned image) ends a TLS-listener connection that does not
// progress: A) TCP only, nothing sent; B) a ClientHello, then nothing; C) a completed mTLS
// handshake, then no MQTT CONNECT. Usage: bun measure.ts <port> <certs dir>
import net from "node:net";
import tls from "node:tls";
import { readFileSync } from "node:fs";

const port = Number(process.argv[2]);
const certs = process.argv[3];
const LIMIT_MS = 400_000;

function waitClose(socket: net.Socket, started: number, label: string): Promise<string> {
  return new Promise((resolve) => {
    let received = 0;
    const timer = setTimeout(() => {
      socket.destroy();
      resolve(`${label}: still open after ${LIMIT_MS} ms (received ${received} bytes)`);
    }, LIMIT_MS);
    socket.on("data", (chunk) => {
      received += chunk.length;
    });
    const done = (how: string) => {
      clearTimeout(timer);
      resolve(`${label}: broker ${how} after ${Date.now() - started} ms (received ${received} bytes)`);
    };
    socket.once("end", () => done("closed (EOF)"));
    socket.once("error", (e) => done(`reset (${(e as Error).message})`));
    socket.once("close", () => done("closed"));
  });
}

// A ClientHello from this runtime's TLS client, captured by a plain TCP listener.
async function captureClientHello(): Promise<Buffer> {
  return new Promise((resolve) => {
    const server = net.createServer((s) => {
      s.once("data", (chunk) => {
        resolve(Buffer.from(chunk));
        s.destroy();
        server.close();
      });
    });
    server.listen(0, "127.0.0.1", () => {
      const address = server.address() as net.AddressInfo;
      const client = tls.connect({ host: "127.0.0.1", port: address.port, rejectUnauthorized: false, minVersion: "TLSv1.3" });
      client.on("error", () => {});
    });
  });
}

async function scenarioA(): Promise<string> {
  const socket = net.connect(port, "127.0.0.1");
  await new Promise((r) => socket.once("connect", r));
  return waitClose(socket, Date.now(), "A tcp-only");
}

async function scenarioB(hello: Buffer): Promise<string> {
  const socket = net.connect(port, "127.0.0.1");
  await new Promise((r) => socket.once("connect", r));
  const started = Date.now();
  socket.write(hello);
  return waitClose(socket, started, "B client-hello-then-silence");
}

async function scenarioC(): Promise<string> {
  const socket = tls.connect({
    host: "127.0.0.1",
    port,
    servername: "localhost",
    ca: readFileSync(`${certs}/ca.pem`),
    cert: readFileSync(`${certs}/client.pem`),
    key: readFileSync(`${certs}/client.key`),
    minVersion: "TLSv1.3",
  });
  await new Promise((r) => socket.once("secureConnect", r));
  return waitClose(socket, Date.now(), "C tls-done-no-connect");
}

const hello = await captureClientHello();
console.log(`captured ClientHello: ${hello.length} bytes, first byte 0x${hello[0].toString(16)}`);
const results = await Promise.all([scenarioA(), scenarioB(hello), scenarioC()]);
for (const line of results) console.log(line);
