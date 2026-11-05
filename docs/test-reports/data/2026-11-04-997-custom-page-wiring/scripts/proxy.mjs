// Forwards localhost:$PORT in the Playwright container to the same port on the host.
import net from "node:net";
const port = Number(process.env.PORT ?? 29197);
net
  .createServer((client) => {
    const upstream = net.connect(port, "host.docker.internal");
    client.pipe(upstream).pipe(client);
    const close = () => {
      client.destroy();
      upstream.destroy();
    };
    client.on("error", close);
    upstream.on("error", close);
  })
  .listen(port, "127.0.0.1");
