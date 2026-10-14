// Raw HTTP/1.1 client of the ingress e2e test (Laysi/trustiot-v2#62, #261): fetch() would
// normalize the request target (dot segments, encodings), which is exactly what the bypass cases
// must send unchanged. Kept apart from ingress.e2e.ts so a unit test can drive it without a
// cluster (raw-client.test.ts, tunnel-recovery.test.ts). The nightly's API check through the
// ingress (scripts/nightly/api.ts, Laysi/trustiot-v2#65) sends its requests with it too.

// ms: time to the first response byte. Envoy keeps the connection open for up to 1 s after a local
// reply (delayed close), so the time until close would overstate how fast the gateway answered.
export type RawResponse = { status: number; headers: Record<string, string[]>; body: string; ms: number };

export type RawEndpoint = { host: string; port: number; tls: boolean; ca?: string };

export type RawRequest = {
  target: string;
  method?: string;
  // Value of the Host header.
  hostHeader: string;
  headers?: [string, string][];
  // TLS server name (on-prem shape); defaults to the Host header without its port.
  sni?: string;
  // Request body, sent with its content-length (the nightly's API check posts JSON with it).
  body?: string;
};

// The request never produced an HTTP response because the connection failed: refused, reset,
// closed before the response head, or timed out. Only this kind of failure may be retried after
// the tunnel to the gateway was lost; anything else, an HTTP status included, is a real result.
// A failure after the response head has arrived is therefore a plain Error, not a ConnectionError.
export class ConnectionError extends Error {
  override name = "ConnectionError";
}

export function decodeChunked(body: Buffer): Buffer {
  const parts: Buffer[] = [];
  let offset = 0;
  while (offset < body.length) {
    const lineEnd = body.indexOf("\r\n", offset);
    if (lineEnd < 0) break;
    const size = Number.parseInt(body.subarray(offset, lineEnd).toString(), 16);
    if (!Number.isFinite(size) || size === 0) break;
    parts.push(body.subarray(lineEnd + 2, lineEnd + 2 + size));
    offset = lineEnd + 2 + size + 2;
  }
  return Buffer.concat(parts);
}

export async function sendRaw(
  endpoint: RawEndpoint,
  request: RawRequest,
  timeoutMs = 15_000,
): Promise<RawResponse> {
  const lines = [
    `${request.method ?? "GET"} ${request.target} HTTP/1.1`,
    `host: ${request.hostHeader}`,
    "user-agent: trustiot-ingress-e2e",
    "connection: close",
    ...(request.headers ?? []).map(([name, value]) => `${name}: ${value}`),
    ...(request.body === undefined ? [] : [`content-length: ${Buffer.byteLength(request.body)}`]),
  ];
  const text = `${lines.join("\r\n")}\r\n\r\n${request.body ?? ""}`;
  const chunks: Buffer[] = [];
  const started = performance.now();
  let firstByte: number | undefined;
  await new Promise<void>((resolve, reject) => {
    // Once the response head has arrived the gateway did answer, and resending could replay a
    // request whose answer the test must see: such a failure is not a ConnectionError.
    const failWith = (message: string, cause?: unknown) => {
      const options = cause === undefined ? undefined : { cause };
      const answered = Buffer.concat(chunks).includes("\r\n\r\n");
      reject(answered ? new Error(`${message} (after the response head)`, options) : new ConnectionError(message, options));
    };
    const timer = setTimeout(() => failWith(`timeout: ${request.target}`), timeoutMs);
    const finish = () => {
      clearTimeout(timer);
      resolve();
    };
    const fail = (error: Error) => {
      clearTimeout(timer);
      failWith(`connection failed for ${request.target}: ${error.message}`, error);
    };
    Bun.connect({
      hostname: endpoint.host,
      port: endpoint.port,
      tls: endpoint.tls
        ? { serverName: request.sni ?? request.hostHeader.split(":")[0], ca: endpoint.ca }
        : undefined,
      socket: {
        open(socket) {
          socket.write(text);
        },
        data(_socket, data) {
          firstByte ??= performance.now();
          chunks.push(Buffer.from(data));
        },
        close: finish,
        end: finish,
        error: (_socket, error) => fail(error),
        connectError: (_socket, error) => fail(error),
      },
    }).catch(fail);
  });
  const ms = (firstByte ?? performance.now()) - started;
  const response = Buffer.concat(chunks);
  const split = response.indexOf("\r\n\r\n");
  if (split < 0) {
    throw new ConnectionError(`no HTTP response for ${request.target}: ${JSON.stringify(response.toString())}`);
  }
  const [statusLine = "", ...headerLines] = response.subarray(0, split).toString().split("\r\n");
  const status = Number(statusLine.split(" ")[1]);
  const headers: Record<string, string[]> = {};
  for (const line of headerLines) {
    const colon = line.indexOf(":");
    const name = line.slice(0, colon).trim().toLowerCase();
    (headers[name] ??= []).push(line.slice(colon + 1).trim());
  }
  // Typed as Buffer, not the inferred Buffer<ArrayBuffer>: decodeChunked returns the wider type.
  let body: Buffer = response.subarray(split + 4);
  if (headers["transfer-encoding"]?.some((v) => v.toLowerCase().includes("chunked"))) body = decodeChunked(body);
  return { status, headers, body: body.toString(), ms };
}
