// A minimal MQTT 3.1.1 client: just enough to check the broker's login, publish a retained
// message and read it back. Packet encoding and decoding are pure and unit tested; the socket
// code is a thin shell around them.
import { connect, type Socket } from "node:net";

const encoder = new TextEncoder();

function utf8(text: string): Uint8Array {
  const bytes = encoder.encode(text);
  if (bytes.length > 0xffff) throw new Error("MQTT string too long");
  return Uint8Array.from([bytes.length >> 8, bytes.length & 0xff, ...bytes]);
}

function remainingLength(length: number): number[] {
  const out: number[] = [];
  let rest = length;
  do {
    let digit = rest % 128;
    rest = Math.floor(rest / 128);
    if (rest > 0) digit |= 0x80;
    out.push(digit);
  } while (rest > 0);
  return out;
}

function packet(first: number, body: Uint8Array[]): Uint8Array {
  const length = body.reduce((sum, part) => sum + part.length, 0);
  return Uint8Array.from([first, ...remainingLength(length), ...body.flatMap((part) => [...part])]);
}

export interface Login {
  user?: string;
  password?: string;
}

export function encodeConnect(clientId: string, login: Login = {}, keepAlive = 30): Uint8Array {
  let flags = 0x02; // clean session
  if (login.user !== undefined) flags |= 0x80;
  if (login.password !== undefined) flags |= 0x40;
  const payload = [utf8(clientId)];
  if (login.user !== undefined) payload.push(utf8(login.user));
  if (login.password !== undefined) payload.push(utf8(login.password));
  return packet(0x10, [
    utf8("MQTT"),
    Uint8Array.from([0x04, flags, keepAlive >> 8, keepAlive & 0xff]),
    ...payload,
  ]);
}

/** QoS 0 PUBLISH. */
export function encodePublish(topic: string, payload: string, retain: boolean): Uint8Array {
  return packet(0x30 | (retain ? 1 : 0), [utf8(topic), encoder.encode(payload)]);
}

export function encodeSubscribe(packetId: number, topic: string): Uint8Array {
  return packet(0x82, [
    Uint8Array.from([packetId >> 8, packetId & 0xff]),
    utf8(topic),
    Uint8Array.from([0x00]),
  ]);
}

export const DISCONNECT = Uint8Array.from([0xe0, 0x00]);

/** CONNACK return codes (MQTT 3.1.1, 3.2.2.3). */
export const CONNACK_ACCEPTED = 0;

export interface Decoded {
  type: number;
  flags: number;
  body: Uint8Array;
  /** Bytes consumed from the buffer. */
  size: number;
}

/** Decodes the first packet in `buffer`, or returns undefined while it is incomplete. */
export function decodePacket(buffer: Uint8Array): Decoded | undefined {
  if (buffer.length < 2) return undefined;
  let length = 0;
  let multiplier = 1;
  let index = 1;
  for (;;) {
    if (index >= buffer.length) return undefined;
    const byte = buffer[index++]!;
    length += (byte & 0x7f) * multiplier;
    if ((byte & 0x80) === 0) break;
    multiplier *= 128;
    if (multiplier > 128 ** 3) throw new Error("malformed MQTT remaining length");
  }
  if (buffer.length < index + length) return undefined;
  return {
    type: buffer[0]! >> 4,
    flags: buffer[0]! & 0x0f,
    body: buffer.slice(index, index + length),
    size: index + length,
  };
}

export function decodePublish(decoded: Decoded): { topic: string; payload: string; retain: boolean } {
  const topicLength = (decoded.body[0]! << 8) | decoded.body[1]!;
  const qos = (decoded.flags >> 1) & 3;
  const start = 2 + topicLength + (qos > 0 ? 2 : 0);
  const decoder = new TextDecoder();
  return {
    topic: decoder.decode(decoded.body.slice(2, 2 + topicLength)),
    payload: decoder.decode(decoded.body.slice(start)),
    retain: (decoded.flags & 1) === 1,
  };
}

/** A connection that yields decoded packets one at a time. */
class Session {
  private buffer: Uint8Array = new Uint8Array(0);
  private waiting: ((packet: Decoded | undefined) => void) | undefined;
  private closed = false;

  constructor(private readonly socket: Socket) {
    socket.on("data", (chunk) => {
      // No encoding is set on the socket, so chunks arrive as bytes; the string case exists
      // only in the type of the "data" event.
      const bytes = typeof chunk === "string" ? encoder.encode(chunk) : chunk;
      const next = new Uint8Array(this.buffer.length + bytes.length);
      next.set(this.buffer);
      next.set(bytes, this.buffer.length);
      this.buffer = next;
      this.flush();
    });
    const close = () => {
      this.closed = true;
      this.flush();
    };
    socket.on("close", close);
    socket.on("error", close);
  }

  private flush(): void {
    if (!this.waiting) return;
    const decoded = decodePacket(this.buffer);
    if (decoded) {
      this.buffer = this.buffer.slice(decoded.size);
      const resolve = this.waiting;
      this.waiting = undefined;
      resolve(decoded);
    } else if (this.closed) {
      const resolve = this.waiting;
      this.waiting = undefined;
      resolve(undefined);
    }
  }

  next(timeoutMs: number): Promise<Decoded | undefined> {
    return new Promise((resolve) => {
      const timer = setTimeout(() => {
        this.waiting = undefined;
        resolve(undefined);
      }, timeoutMs);
      this.waiting = (decoded) => {
        clearTimeout(timer);
        resolve(decoded);
      };
      this.flush();
    });
  }

  send(bytes: Uint8Array): void {
    this.socket.write(bytes);
  }

  end(): void {
    this.socket.destroy();
  }
}

export interface Endpoint {
  host: string;
  port: number;
}

async function open(endpoint: Endpoint, timeoutMs: number): Promise<Session> {
  return await new Promise((resolve, reject) => {
    const socket = connect({ host: endpoint.host, port: endpoint.port });
    const timer = setTimeout(() => {
      socket.destroy();
      reject(new Error(`timed out connecting to ${endpoint.host}:${endpoint.port}`));
    }, timeoutMs);
    socket.once("connect", () => {
      clearTimeout(timer);
      resolve(new Session(socket));
    });
    socket.once("error", (error) => {
      clearTimeout(timer);
      reject(error);
    });
  });
}

/** The CONNACK return code the broker answers a login with (0 means accepted). */
export async function connackCode(
  endpoint: Endpoint,
  login: Login,
  timeoutMs = 5000,
): Promise<number> {
  const session = await open(endpoint, timeoutMs);
  try {
    session.send(encodeConnect("trustiot-dev-check", login));
    const reply = await session.next(timeoutMs);
    if (!reply || reply.type !== 2 || reply.body.length !== 2) {
      // Closing without a CONNACK is how a broker refuses some malformed or unauthorized logins.
      return -1;
    }
    return reply.body[1]!;
  } finally {
    session.end();
  }
}

export async function publishRetained(
  endpoint: Endpoint,
  login: Login,
  topic: string,
  payload: string,
  timeoutMs = 5000,
): Promise<void> {
  const session = await open(endpoint, timeoutMs);
  try {
    session.send(encodeConnect("trustiot-dev-seed", login));
    const reply = await session.next(timeoutMs);
    if (!reply || reply.type !== 2 || reply.body[1] !== CONNACK_ACCEPTED) {
      throw new Error("the MQTT broker refused the login");
    }
    session.send(encodePublish(topic, payload, true));
    session.send(DISCONNECT);
  } finally {
    // The DISCONNECT is flushed before the socket closes.
    setTimeout(() => session.end(), 50);
  }
}

/** The retained message on `topic`, or undefined when there is none. */
export async function readRetained(
  endpoint: Endpoint,
  login: Login,
  topic: string,
  timeoutMs = 5000,
): Promise<string | undefined> {
  const session = await open(endpoint, timeoutMs);
  try {
    session.send(encodeConnect("trustiot-dev-read", login));
    const reply = await session.next(timeoutMs);
    if (!reply || reply.type !== 2 || reply.body[1] !== CONNACK_ACCEPTED) {
      throw new Error("the MQTT broker refused the login");
    }
    session.send(encodeSubscribe(1, topic));
    const deadline = Date.now() + timeoutMs;
    for (;;) {
      const remaining = deadline - Date.now();
      if (remaining <= 0) return undefined;
      // SUBACK (type 9) arrives first; a retained PUBLISH (type 3) follows right after it.
      const next = await session.next(Math.min(remaining, 500));
      if (!next) return undefined;
      if (next.type === 3) return decodePublish(next).payload;
    }
  } finally {
    session.end();
  }
}
