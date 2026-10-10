import { describe, expect, test } from "bun:test";
import { pbkdf2Sync } from "node:crypto";

import {
  dotenv,
  generateCredentials,
  mosquittoPasswordEntry,
  randomSecret,
  sqlIdentifier,
  sqlLiteral,
} from "../lib/secrets.ts";

describe("randomSecret", () => {
  test("is URL-safe and carries 256 bits by default", () => {
    const secret = randomSecret();
    expect(secret).toMatch(/^[A-Za-z0-9_-]{43}$/);
  });

  test("never repeats", () => {
    const seen = new Set(Array.from({ length: 2000 }, () => randomSecret()));
    expect(seen.size).toBe(2000);
  });

  test("refuses a weak length", () => {
    expect(() => randomSecret(8)).toThrow();
  });
});

describe("generateCredentials", () => {
  test("gives every login its own strong password, different on every call", () => {
    const first = generateCredentials();
    const second = generateCredentials();
    const passwords = [
      first.postgres.password,
      first.clickhouse.password,
      first.mqtt.password,
      second.postgres.password,
      second.clickhouse.password,
      second.mqtt.password,
    ];
    expect(new Set(passwords).size).toBe(6);
    for (const password of passwords) expect(password.length).toBeGreaterThanOrEqual(43);
  });
});

describe("mosquittoPasswordEntry", () => {
  test("uses the sha512-pbkdf2 format and verifies against the password", () => {
    const salt = Buffer.from("0123456789ab");
    const entry = mosquittoPasswordEntry("dev", "s3cret", salt);
    const match = /^dev:\$7\$101\$([^$]+)\$([^$]+)\n$/.exec(entry);
    expect(match).not.toBeNull();
    expect(Buffer.from(match![1]!, "base64").equals(salt)).toBe(true);
    const expected = pbkdf2Sync("s3cret", salt, 101, 64, "sha512").toString("base64");
    expect(match![2]).toBe(expected);
  });

  test("does not contain the password", () => {
    expect(mosquittoPasswordEntry("dev", "plain-text-password")).not.toContain("plain-text-password");
  });

  test("rejects a user name that would break the file format", () => {
    expect(() => mosquittoPasswordEntry("a:b", "x")).toThrow();
  });
});

describe("dotenv", () => {
  test("writes KEY=value lines", () => {
    expect(dotenv({ A: "x", B: 5 })).toBe("A=x\nB=5\n");
  });

  test("refuses values and names that need quoting", () => {
    expect(() => dotenv({ A: "a b" })).toThrow();
    expect(() => dotenv({ A: "a\nB=c" })).toThrow();
    expect(() => dotenv({ "A B": "x" })).toThrow();
  });

  test("accepts every generated secret", () => {
    const { postgres, clickhouse, mqtt } = generateCredentials();
    expect(() => dotenv({ A: postgres.password, B: clickhouse.password, C: mqtt.password })).not.toThrow();
  });
});

describe("SQL quoting", () => {
  test("doubles quotes", () => {
    expect(sqlLiteral("it's")).toBe("'it''s'");
    expect(sqlIdentifier('a"b')).toBe('"a""b"');
  });
});
