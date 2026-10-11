// Stand-in for `kubectl port-forward` in the unit tests of the tunnel recovery
// (tunnel-recovery.test.ts, Laysi/trustiot-v2#261): a TCP proxy in a child process of its own.
//
//   bun fake-port-forward.ts <listen-port> <upstream-port> forward
//   bun fake-port-forward.ts <listen-port> <upstream-port> die-on-request [exit-delay-ms|on-stdin]
//
// `forward` relays bytes to 127.0.0.1:<upstream-port> until it is killed. `die-on-request` drops
// the client connection when the first request bytes arrive and exits the process
// <exit-delay-ms> later (default 300): the connection fails first and the process only counts as
// exited afterwards, the way kubectl behaves when it loses the connection to the pod. With
// `on-stdin` the process exits when it reads a line from its stdin instead of after a delay, so
// a test can look at the process between the failed connection and its exit without racing a
// timer on a busy machine.
type Relay = { upstream?: Bun.Socket; pending: Uint8Array[] };

const [listenArg, upstreamArg, mode, delayArg] = process.argv.slice(2);
const listenPort = Number(listenArg);
const upstreamPort = Number(upstreamArg);
const exitOnStdin = delayArg === "on-stdin";
const exitDelayMs = exitOnStdin ? 0 : Number(delayArg ?? "300");
if (!Number.isInteger(listenPort) || !Number.isInteger(upstreamPort) || !Number.isInteger(exitDelayMs) || (mode !== "forward" && mode !== "die-on-request")) {
  console.error("usage: fake-port-forward.ts <listen-port> <upstream-port> forward|die-on-request [exit-delay-ms|on-stdin]");
  process.exit(2);
}

Bun.listen<Relay>({
  hostname: "127.0.0.1",
  port: listenPort,
  socket: {
    open(client) {
      client.data = { pending: [] };
      if (mode === "die-on-request") return;
      void Bun.connect({
        hostname: "127.0.0.1",
        port: upstreamPort,
        socket: {
          open(upstream) {
            client.data.upstream = upstream;
            for (const chunk of client.data.pending) upstream.write(chunk);
            client.data.pending = [];
          },
          data(_upstream, data) {
            client.write(data);
          },
          close() {
            client.end();
          },
          error() {
            client.terminate();
          },
        },
      }).catch(() => client.terminate());
    },
    data(client, data) {
      if (mode === "die-on-request") {
        client.terminate();
        if (exitOnStdin) void Bun.stdin.stream().getReader().read().then(() => process.exit(1));
        else setTimeout(() => process.exit(1), exitDelayMs);
        return;
      }
      if (client.data.upstream) client.data.upstream.write(data);
      else client.data.pending.push(new Uint8Array(data));
    },
    close(client) {
      client.data.upstream?.end();
    },
    error() {},
  },
});
