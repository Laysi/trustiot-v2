// Does a trickle of bytes keep a non-progressing connection open past Mosquitto's 30 s?
// D) the ClientHello one byte every 5 s from the start; E) a full ClientHello, then one byte
// every 5 s. Usage: bun trickle.ts <port>
import net from "node:net";
import tls from "node:tls";
const port = Number(process.argv[2]);
const LIMIT_MS = 200_000;
async function hello(): Promise<Buffer> {
  return new Promise((resolve) => {
    const server = net.createServer((s) => s.once("data", (c) => { resolve(Buffer.from(c)); s.destroy(); server.close(); }));
    server.listen(0, "127.0.0.1", () => {
      const c = tls.connect({ host: "127.0.0.1", port: (server.address() as net.AddressInfo).port, rejectUnauthorized: false });
      c.on("error", () => {});
    });
  });
}
function run(label: string, first: Buffer, trickle: Buffer): Promise<string> {
  return new Promise((resolve) => {
    const socket = net.connect(port, "127.0.0.1");
    let sent = 0;
    let received = 0;
    let timer: ReturnType<typeof setInterval>;
    socket.once("connect", () => {
      const started = Date.now();
      if (first.length) socket.write(first);
      timer = setInterval(() => { if (sent < trickle.length) socket.write(trickle.subarray(sent, ++sent)); }, 5000);
      const end = setTimeout(() => { clearInterval(timer); socket.destroy(); resolve(`${label}: still open after ${LIMIT_MS} ms, ${sent} trickled bytes`); }, LIMIT_MS);
      socket.on("data", (c) => { received += c.length; });
      socket.once("close", () => { clearInterval(timer); clearTimeout(end); resolve(`${label}: broker closed after ${Date.now() - started} ms (${sent} trickled bytes, received ${received})`); });
      socket.on("error", () => {});
    });
  });
}
const ch = await hello();
console.log((await Promise.all([
  run("D client-hello-1-byte-per-5s", Buffer.alloc(0), ch),
  run("E client-hello-then-1-byte-per-5s", ch, Buffer.from([0x17, 0x03, 0x03, 0x40, 0x00, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0])),
])).join("\n"));
