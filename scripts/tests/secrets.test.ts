import { describe, expect, test } from "bun:test";
import { pbkdf2Sync } from "node:crypto";

import {
  dotenv,
  generateCredentials,
  mosquittoPasswordEntry,
  randomSecret,
  serviceRoleSql,
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

  test("makes a login per service role, never the dev user's", () => {
    const credentials = generateCredentials({ "example-service": "example_service" });
    const login = credentials.services["example-service"]!;
    expect(login.user).toBe("example_service");
    expect(login.user).not.toBe(credentials.postgres.user);
    expect(login.password.length).toBeGreaterThanOrEqual(43);
    expect(login.password).not.toBe(credentials.postgres.password);
    expect(generateCredentials().services).toEqual({});
  });
});

describe("serviceRoleSql", () => {
  test("creates a plain login role that cannot bypass row-level security, and sets its password", () => {
    const sql = serviceRoleSql("example_service", "p'w");
    expect(sql).toContain("CREATE ROLE \"example_service\" LOGIN NOSUPERUSER NOBYPASSRLS");
    expect(sql).toContain("NOCREATEDB NOCREATEROLE NOREPLICATION");
    expect(sql).toContain("ALTER ROLE \"example_service\" WITH LOGIN NOSUPERUSER NOBYPASSRLS");
    expect(sql).toContain("PASSWORD 'p''w'");
    expect(sql).not.toMatch(/GRANT|OWNER/i);
  });

  test("can run again over an existing role", () => {
    expect(serviceRoleSql("r", "x")).toContain("IF NOT EXISTS (SELECT FROM pg_roles");
  });

  test("refuses a role name that is not a plain identifier", () => {
    for (const bad of ["", "Example", "a-b", "a b", "1a", 'a"; DROP ROLE x; --', "x".repeat(64)]) {
      expect(() => serviceRoleSql(bad, "x")).toThrow(/invalid database role name/);
    }
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

describe("dotenv with quoted JSON values", () => {
  test("single-quotes only the named keys, so a shell eval and a dotenv reader get the JSON back", () => {
    const json = '{"keys":[{"kty":"OKP","kid":"a-b_c","x":"abc"}]}';
    const text = dotenv({ A: "x", KEYS: json }, { quoted: ["KEYS"] });
    expect(text).toBe(`A=x\nKEYS='${json}'\n`);
  });

  test("refuses a quoted value that a single quote cannot hold", () => {
    expect(() => dotenv({ A: "it's" }, { quoted: ["A"] })).toThrow(/cannot be single-quoted/);
    expect(() => dotenv({ A: "a\nb" }, { quoted: ["A"] })).toThrow(/cannot be single-quoted/);
  });

  test("still refuses unquoted JSON, which podman's env file would read literally", () => {
    expect(() => dotenv({ A: '{"a":1}' })).toThrow();
  });
});

describe("SQL quoting", () => {
  test("doubles quotes", () => {
    expect(sqlLiteral("it's")).toBe("'it''s'");
    expect(sqlIdentifier('a"b')).toBe('"a""b"');
  });
});
