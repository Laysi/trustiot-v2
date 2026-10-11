// Local ports for the tunnels of the ingress e2e test (Laysi/trustiot-v2#288). Kept apart from
// cluster.ts, which needs the e2e environment variables at import time, so unit tests can use it.

// True while something accepts connections on 127.0.0.1:<port>.
async function accepts(port: number): Promise<boolean> {
  try {
    const socket = await Bun.connect({ hostname: "127.0.0.1", port, socket: { data() {} } });
    socket.end();
    return true;
  } catch {
    return false;
  }
}

// A port on 127.0.0.1 that nothing listens on, for a tunnel process that binds it next.
//
// Listener.stop(true) does not close the listening socket before it returns: on WSL2 (Bun 1.4.2,
// kernel 6.18) the socket keeps accepting connections until the event loop has run once more.
// A caller that spawned a tunnel and probed the port at once reached this dying listener, not
// the tunnel, saw the port as "ready", and then sent its request into a closed socket. So the
// port is only handed out after connections to it are refused.
export async function freePort(): Promise<number> {
  for (let attempt = 0; attempt < 5; attempt++) {
    const server = Bun.listen({ hostname: "127.0.0.1", port: 0, socket: { data() {} } });
    const port = server.port;
    server.stop(true);
    const deadline = Date.now() + 2_000;
    while ((await accepts(port)) && Date.now() < deadline) await Bun.sleep(5);
    // A listener that still accepts after the deadline belongs to someone else: pick another port.
    if (!(await accepts(port))) return port;
  }
  throw new Error("no local port without a listener found");
}
