import { describe, expect, test } from "bun:test";

import { z } from "./zod.ts";

const SUBPROCESS_TIMEOUT_MS = 60_000;

describe("the zod wrapper", () => {
  test("zod runs in jitless mode once the wrapper is imported", () => {
    expect(z.config().jitless).toBe(true);
  });

  test("building and running object schemas never compiles code with `Function`", () => {
    const real = globalThis.Function;
    let calls = 0;
    // A construct trap counts what `new Function(...)` would run; zod captures `Function` when
    // it probes, so the stand-in has to be in place before the schema is created.
    globalThis.Function = new Proxy(real, {
      construct(target, args, newTarget) {
        calls += 1;
        return Reflect.construct(target, args, newTarget);
      },
    });
    try {
      const schema = z.object({ a: z.string(), b: z.object({ c: z.number() }).optional() });
      expect(schema.safeParse({ a: "x", b: { c: 1 } }).success).toBe(true);
      expect(schema.safeParse({ a: 1 }).success).toBe(false);
    } finally {
      globalThis.Function = real;
    }
    expect(calls).toBe(0);
  });

  test(
    "importing the wrapper first keeps zod from probing with `Function`, from the very first schema",
    async () => {
      // A fresh process, so no schema was created earlier in this one: the probe, if there were
      // one, would run on the first `z.object`.
      const script = `
      const real = globalThis.Function;
      let calls = 0;
      globalThis.Function = new Proxy(real, { construct(t, a, n) { calls += 1; return Reflect.construct(t, a, n); } });
      const { z } = await import(${JSON.stringify(new URL("./zod.ts", import.meta.url).pathname)});
      z.object({ a: z.string() }).safeParse({ a: "x" });
      console.log(JSON.stringify({ calls, jitless: z.config().jitless }));
    `;
      const child = Bun.spawn([process.execPath, "-e", script], { stdout: "pipe", stderr: "pipe" });
      const [out, err, code] = await Promise.all([
        new Response(child.stdout).text(),
        new Response(child.stderr).text(),
        child.exited,
      ]);
      expect({ code, err }).toEqual({ code: 0, err: "" });
      expect(JSON.parse(out)).toEqual({ calls: 0, jitless: true });
    },
    SUBPROCESS_TIMEOUT_MS,
  );

  test(
    "without jitless the probe does run, so the checks above can fail",
    async () => {
      // The control: the same script importing zod directly (this file is not linted for it
      // because it is a string) sees the probe.
      const script = `
      const real = globalThis.Function;
      let calls = 0;
      globalThis.Function = new Proxy(real, { construct(t, a, n) { calls += 1; return Reflect.construct(t, a, n); } });
      const { z } = await import("zod");
      z.object({ a: z.string() }).safeParse({ a: "x" });
      console.log(JSON.stringify({ calls: calls > 0 }));
    `;
      const child = Bun.spawn([process.execPath, "-e", script], {
        cwd: new URL("../..", import.meta.url).pathname,
        stdout: "pipe",
        stderr: "pipe",
      });
      const [out, code] = await Promise.all([new Response(child.stdout).text(), child.exited]);
      expect(code).toBe(0);
      expect(JSON.parse(out)).toEqual({ calls: true });
    },
    SUBPROCESS_TIMEOUT_MS,
  );
});
