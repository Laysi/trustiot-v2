// Unit test of freePort (Laysi/trustiot-v2#288). Runs with `bun test deploy/chart`.
import { expect, test } from "bun:test";
import { freePort } from "./ports.ts";

test("nothing accepts connections on a port right after freePort() returns it", async () => {
  // Before #288 the stopped listener kept accepting for a moment on WSL2.
  for (let i = 0; i < 20; i++) {
    const port = await freePort();
    await expect(Bun.connect({ hostname: "127.0.0.1", port, socket: { data() {} } })).rejects.toThrow();
  }
});

test("the returned port can be bound at once", async () => {
  const port = await freePort();
  const server = Bun.listen({ hostname: "127.0.0.1", port, socket: { data() {} } });
  try {
    expect(server.port).toBe(port);
  } finally {
    server.stop(true);
  }
});
