import { describe, expect, test } from "bun:test";

import {
  decodePacket,
  decodePublish,
  encodeConnect,
  encodePublish,
  encodeSubscribe,
} from "../lib/mqtt.ts";
import { authRequest, AUTH_OK, AUTH_SASL, startupMessage } from "../lib/pgwire.ts";

const bytes = (...values: number[]) => Uint8Array.from(values);

describe("MQTT packets", () => {
  test("CONNECT with a login matches the 3.1.1 layout", () => {
    const packet = encodeConnect("c", { user: "u", password: "p" }, 30);
    expect([...packet]).toEqual([
      0x10, 0x13, // CONNECT, remaining length 19
      0x00, 0x04, 0x4d, 0x51, 0x54, 0x54, // "MQTT"
      0x04, 0xc2, 0x00, 0x1e, // level 4, flags username+password+clean, keep alive 30
      0x00, 0x01, 0x63, // client id "c"
      0x00, 0x01, 0x75, // user "u"
      0x00, 0x01, 0x70, // password "p"
    ]);
  });

  test("an anonymous CONNECT sets no login flags", () => {
    const packet = encodeConnect("c");
    expect(packet[9]).toBe(0x02);
  });

  test("remaining length uses the variable-length encoding", () => {
    const payload = "x".repeat(300);
    const packet = encodePublish("t", payload, true);
    expect(packet[0]).toBe(0x31);
    // 2 + 1 + 300 = 303 = 0b1_0010_1111 -> 0xAF 0x02
    expect([packet[1], packet[2]]).toEqual([0xaf, 0x02]);
    const decoded = decodePacket(packet)!;
    expect(decoded.size).toBe(packet.length);
    expect(decodePublish(decoded)).toEqual({ topic: "t", payload, retain: true });
  });

  test("SUBSCRIBE carries the packet id, the filter and QoS 0", () => {
    expect([...encodeSubscribe(1, "a/b")]).toEqual([0x82, 0x08, 0x00, 0x01, 0x00, 0x03, 0x61, 0x2f, 0x62, 0x00]);
  });

  test("decodePacket waits for a complete packet and consumes only one", () => {
    const connack = bytes(0x20, 0x02, 0x00, 0x05);
    expect(decodePacket(bytes(0x20))).toBeUndefined();
    expect(decodePacket(bytes(0x20, 0x02, 0x00))).toBeUndefined();
    const two = Uint8Array.from([...connack, ...connack]);
    const first = decodePacket(two)!;
    expect(first.type).toBe(2);
    expect([...first.body]).toEqual([0x00, 0x05]);
    expect(first.size).toBe(4);
  });

  test("a malformed length is an error", () => {
    expect(() => decodePacket(bytes(0x30, 0xff, 0xff, 0xff, 0xff, 0x01))).toThrow();
  });
});

describe("Postgres startup", () => {
  test("the StartupMessage has the protocol version and the parameters", () => {
    const message = startupMessage("u", "d");
    const view = new DataView(message.buffer);
    expect(view.getInt32(0)).toBe(message.length);
    expect(view.getInt32(4)).toBe(196608);
    expect(new TextDecoder().decode(message.slice(8))).toBe("user\0u\0database\0d\0\0");
  });

  test("authRequest reads the request type", () => {
    const sasl = bytes(0x52, 0, 0, 0, 23, 0, 0, 0, 10, 1, 2);
    const ok = bytes(0x52, 0, 0, 0, 8, 0, 0, 0, 0);
    expect(authRequest(sasl)).toBe(AUTH_SASL);
    expect(authRequest(ok)).toBe(AUTH_OK);
    expect(authRequest(bytes(0x52, 0, 0))).toBeUndefined();
    expect(authRequest(bytes())).toBeUndefined();
    expect(authRequest(bytes(0x45, 0, 0, 0, 5))).toBe("error");
    expect(() => authRequest(bytes(0x00))).toThrow();
  });
});
