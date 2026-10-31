// harden (frontend/sandbox/src/harden.ts) freezes a realm's ECMAScript intrinsics so the
// display-time comparison reads genuine built-ins whatever the page does. A realm is the unit here,
// so these tests run harden in a fresh `node:vm` context — its own intrinsics, like a browser
// frame's — and never touch the test process's own built-ins.
import { describe, expect, test } from "bun:test";
import { join } from "node:path";
import vm from "node:vm";

const ENTRIES = new URL("../src/", import.meta.url);

async function bundle(file: string, format: "cjs" | "iife", target: "node" | "browser"): Promise<string> {
  const built = await Bun.build({ entrypoints: [join(new URL(ENTRIES).pathname, file)], format, target });
  const output = built.outputs[0];
  if (!built.success || output === undefined) throw new AggregateError(built.logs, `bundling ${file} failed`);
  // `node:vm` runs a classic script, where `import.meta` is a syntax error; the sandbox code does
  // not use it, so a bundle that mentions it (d3's wrapper) runs the same with a plain object.
  return (await output.text()).replace(/\bimport\.meta\b/g, "({url:''})");
}

/** A fresh realm with `harden` installed as a global, like the sandbox frame after runtime.js loads. */
async function hardenedRealm(withD3 = false): Promise<vm.Context> {
  const context = vm.createContext({});
  vm.runInContext("globalThis.module = { exports: {} }; globalThis.exports = globalThis.module.exports;", context);
  vm.runInContext(await bundle("harden.ts", "cjs", "node"), context);
  vm.runInContext(
    "globalThis.harden = module.exports.harden; delete globalThis.module; delete globalThis.exports;",
    context,
  );
  if (withD3) vm.runInContext(await bundle("entries/d3.ts", "iife", "browser"), context);
  return context;
}

/** Runs `expression` in the realm and returns its JSON value. */
function evaluate<T>(context: vm.Context, expression: string): T {
  return JSON.parse(vm.runInContext(`JSON.stringify((() => { ${expression} })())`, context)) as T;
}

describe("harden", () => {
  test("a comparison that looks its built-ins up at call time is immune to a page replacing them", async () => {
    const context = await hardenedRealm();
    const result = evaluate<{
      before: unknown;
      after: unknown;
      immune: boolean;
      attempts: { threw: number; ignored: number };
    }>(
      context,
      `
      // Looks every built-in up at call time, like compare.ts, format.ts, time.ts and geometry.ts.
      const measure = (text) => ({
        literal: (/[0-9][0-9.,]*/.exec(text) ?? [null])[0],
        upper: String.prototype.toUpperCase.call(text),
        rounded: Math.round(3.7),
        joined: Array.from([1, 2, 3]).join("-"),
        has: new Set([7]).has(7),
        parsed: Number.parseInt("42px", 10),
        formatted: new Intl.NumberFormat("en-US").format(1234.5),
        time: new Date(Date.UTC(2020, 0, 1)).getTime(),
        mapped: (() => { const m = new Map(); m.set("k", 9); return m.get("k"); })(),
      });
      const before = measure("abc 1,234 def");
      harden();
      // Everything a page could try, to make its own tq.text and the comparison agree on a lie:
      // a method on an intrinsic, and the whole intrinsic behind a bare reference.
      let threw = 0;
      let ignored = 0;
      const attempt = (fn) => { try { fn(); ignored++; } catch { threw++; } };
      attempt(() => { "use strict"; Math.round = () => 0; });
      attempt(() => { "use strict"; Number.parseInt = () => 0; });
      attempt(() => { "use strict"; Intl.NumberFormat = function () { return { format: () => "0" }; }; });
      attempt(() => { "use strict"; String.prototype.toUpperCase = () => "X"; });
      attempt(() => { "use strict"; RegExp.prototype.exec = () => null; });
      attempt(() => { "use strict"; Array.from = () => []; });
      attempt(() => { "use strict"; Set.prototype.has = () => false; });
      attempt(() => { "use strict"; Map.prototype.get = () => 0; });
      attempt(() => { "use strict"; Date.prototype.getTime = () => 0; });
      // Replacing the bare binding itself (globalThis.Math, Math = ...).
      attempt(() => { "use strict"; globalThis.Math = { round: () => 0 }; });
      attempt(() => { "use strict"; globalThis.Array = { from: () => [] }; });
      attempt(() => { "use strict"; globalThis.Intl = { NumberFormat: function () { return { format: () => "0" }; } }; });
      attempt(() => { "use strict"; globalThis.Number = { parseInt: () => 0 }; });
      attempt(() => { "use strict"; globalThis.Set = function () { return { has: () => false }; }; });
      const after = measure("abc 1,234 def");
      return { before, after, immune: JSON.stringify(before) === JSON.stringify(after), attempts: { threw, ignored } };
    `,
    );
    expect(result.immune).toBe(true);
    expect(result.after).toEqual(result.before);
    // Replacing a static method or a locked binding throws in strict mode; replacing a prototype
    // method is silently refused by the override setter. None of it reaches the comparison.
    expect(result.attempts.threw).toBeGreaterThan(0);
    expect(result.attempts.threw + result.attempts.ignored).toBe(14);
  });

  test("a page cannot replace a method on a frozen prototype, but can still shadow it on its own objects", async () => {
    const context = await hardenedRealm();
    const result = evaluate<{
      strict: boolean;
      prototypeUnchanged: boolean;
      ownShadow: string;
      genuineUnaffected: string;
    }>(
      context,
      `
      "use strict";
      // The directive is the body's first statement, so the body is strict: a plain call has no this.
      const strict = (function () { return this === undefined; })();
      harden();
      const genuine = String.prototype.toUpperCase;
      try { String.prototype.toUpperCase = () => "X"; } catch {}
      const prototypeUnchanged = String.prototype.toUpperCase === genuine && "ab".toUpperCase() === "AB";
      // A wrapper object may shadow the method on itself (the override mistake stays fixed).
      const wrapper = Object.create(String.prototype);
      wrapper.toUpperCase = () => "shadowed";
      return { strict, prototypeUnchanged, ownShadow: wrapper.toUpperCase(), genuineUnaffected: "cd".toUpperCase() };
    `,
    );
    expect(result.strict).toBe(true);
    expect(result.prototypeUnchanged).toBe(true);
    expect(result.ownShadow).toBe("shadowed");
    expect(result.genuineUnaffected).toBe("CD");
  });

  test("the override mistake is fixed: assigning an inherited property name still works in strict mode", async () => {
    const context = await hardenedRealm();
    const result = evaluate<Record<string, unknown>>(
      context,
      `
      "use strict";
      harden();
      const out = { strict: (function () { return this === undefined; })() };
      const ok = (name, fn) => { try { out[name] = fn(); } catch (e) { out[name] = "threw: " + e.message; } };
      ok("objectToString", () => { const o = {}; o.toString = () => "x"; return o.toString(); });
      ok("errorName", () => { const e = new Error("m"); e.name = "Custom"; e.message = "mm"; return e.name + "/" + e.message; });
      ok("classMethod", () => { class A { toString() { return "A"; } } return new A().toString(); });
      ok("iterate", () => { let s = 0; for (const x of [1, 2, 3]) s += x; return s; });
      ok("spread", () => [...[1, 2], ...[3]].length);
      ok("mapSet", () => { const m = new Map(); m.set("k", 1); return m.get("k"); });
      ok("defineOnArray", () => { const a = [1]; a.custom = 2; return a.custom; });
      return out;
    `,
    );
    expect(result).toEqual({
      strict: true,
      objectToString: "x",
      errorName: "Custom/mm",
      classMethod: "A",
      iterate: 6,
      spread: 3,
      mapSet: 1,
      defineOnArray: 2,
    });
  });

  test("freezes the intrinsics and their prototypes, and leaves the global object extensible", async () => {
    const context = await hardenedRealm();
    const result = evaluate<Record<string, boolean>>(
      context,
      `
      harden();
      return {
        objectPrototype: Object.isFrozen(Object.prototype),
        arrayPrototype: Object.isFrozen(Array.prototype),
        functionPrototype: Object.isFrozen(Function.prototype),
        math: Object.isFrozen(Math),
        json: Object.isFrozen(JSON),
        reflect: Object.isFrozen(Reflect),
        regexp: Object.isFrozen(RegExp.prototype),
        date: Object.isFrozen(Date.prototype),
        set: Object.isFrozen(Set.prototype),
        intl: Object.isFrozen(Intl.DateTimeFormat.prototype),
        arrayIterator: Object.isFrozen(Object.getPrototypeOf([][Symbol.iterator]())),
        globalExtensible: Object.isExtensible(globalThis),
      };
    `,
    );
    expect(result).toEqual({
      objectPrototype: true,
      arrayPrototype: true,
      functionPrototype: true,
      math: true,
      json: true,
      reflect: true,
      regexp: true,
      date: true,
      set: true,
      intl: true,
      arrayIterator: true,
      globalExtensible: true,
    });
  });

  test("freezes every well-known intrinsic, those no global names included", async () => {
    const context = await hardenedRealm();
    const result = evaluate<{ missing: string[]; notFrozen: string[] }>(
      context,
      `
      // Reached before harden runs, the way a page would reach them.
      const proto = Object.getPrototypeOf;
      const wellKnown = {
        "%Object.prototype%": Object.prototype,
        "%Function.prototype%": Function.prototype,
        "%Array.prototype%": Array.prototype,
        "%Array%": Array,
        "%String.prototype%": String.prototype,
        "%Number.prototype%": Number.prototype,
        "%Boolean.prototype%": Boolean.prototype,
        "%Symbol.prototype%": Symbol.prototype,
        "%BigInt.prototype%": BigInt.prototype,
        "%Date.prototype%": Date.prototype,
        "%RegExp.prototype%": RegExp.prototype,
        "%Error.prototype%": Error.prototype,
        "%TypeError.prototype%": TypeError.prototype,
        "%RangeError.prototype%": RangeError.prototype,
        "%Map.prototype%": Map.prototype,
        "%Set.prototype%": Set.prototype,
        "%WeakMap.prototype%": WeakMap.prototype,
        "%WeakSet.prototype%": WeakSet.prototype,
        "%Promise.prototype%": Promise.prototype,
        "%ArrayBuffer.prototype%": ArrayBuffer.prototype,
        "%DataView.prototype%": DataView.prototype,
        "%TypedArray.prototype%": proto(Uint8Array.prototype),
        "%Float64Array.prototype%": Float64Array.prototype,
        "%Math%": Math,
        "%JSON%": JSON,
        "%Reflect%": Reflect,
        "%Intl%": Intl,
        "%Intl.NumberFormat.prototype%": Intl.NumberFormat.prototype,
        "%Intl.DateTimeFormat.prototype%": Intl.DateTimeFormat.prototype,
        "%Intl.PluralRules.prototype%": Intl.PluralRules.prototype,
        "%IteratorPrototype%": proto(proto([][Symbol.iterator]())),
        "%ArrayIteratorPrototype%": proto([][Symbol.iterator]()),
        "%MapIteratorPrototype%": proto(new Map()[Symbol.iterator]()),
        "%SetIteratorPrototype%": proto(new Set()[Symbol.iterator]()),
        "%StringIteratorPrototype%": proto(""[Symbol.iterator]()),
        "%RegExpStringIteratorPrototype%": proto("".matchAll(/(?:)/g)),
        "%GeneratorFunction.prototype%": proto(function* () {}),
        "%GeneratorPrototype%": proto(proto((function* () {})())),
        "%AsyncGeneratorFunction.prototype%": proto(async function* () {}),
        "%AsyncGeneratorPrototype%": proto(proto((async function* () {})())),
        "%AsyncIteratorPrototype%": proto(proto(proto((async function* () {})()))),
        "%AsyncFunction.prototype%": proto(async () => {}),
      };
      if (typeof Iterator === "function") {
        wellKnown["%Iterator%"] = Iterator;
        wellKnown["%IteratorHelperPrototype%"] = proto([][Symbol.iterator]().map((value) => value));
        wellKnown["%WrapForValidIteratorPrototype%"] = proto(Iterator.from({ next: () => ({ done: true }) }));
      }
      harden();
      const missing = Object.keys(wellKnown).filter((name) => wellKnown[name] == null);
      const notFrozen = Object.keys(wellKnown).filter((name) => wellKnown[name] != null && !Object.isFrozen(wellKnown[name]));
      return { missing, notFrozen };
    `,
    );
    expect(result).toEqual({ missing: [], notFrozen: [] });
  });

  test("locks the global bindings of the intrinsics but leaves other globals and new ones free", async () => {
    const context = await hardenedRealm();
    const result = evaluate<Record<string, unknown>>(
      context,
      `
      const locked = (name) => {
        const d = Object.getOwnPropertyDescriptor(globalThis, name);
        return d !== undefined && d.writable === false && d.configurable === false;
      };
      harden();
      // A page may still add its own globals, as the runtime adds tq.
      let addedOwn = false;
      try { globalThis.pageWidget = 1; addedOwn = globalThis.pageWidget === 1; } catch {}
      return {
        math: locked("Math"),
        array: locked("Array"),
        intl: locked("Intl"),
        number: locked("Number"),
        parseInt: locked("parseInt"),
        json: locked("JSON"),
        addedOwn,
      };
    `,
    );
    expect(result).toEqual({
      math: true,
      array: true,
      intl: true,
      number: true,
      parseInt: true,
      json: true,
      addedOwn: true,
    });
  });

  test("runs once and is safe to call again in an already-frozen realm", async () => {
    const context = await hardenedRealm();
    const result = evaluate<{ secondThrew: boolean; stillWorks: string }>(
      context,
      `
      harden();
      let secondThrew = false;
      try { harden(); } catch { secondThrew = true; }
      return { secondThrew, stillWorks: "ab".toUpperCase() };
    `,
    );
    expect(result.secondThrew).toBe(false);
    expect(result.stillWorks).toBe("AB");
  });

  test("the bundled d3 still builds scales, nice domains, arcs and formats under the frozen realm", async () => {
    const context = await hardenedRealm(true);
    const result = evaluate<Record<string, unknown>>(
      context,
      `
      harden();
      const linear = d3.scaleLinear().domain([0, 97]).range([0, 500]).nice();
      const timeDomain = d3
        .scaleUtc()
        .domain([new Date(Date.UTC(2020, 0, 1)), new Date(Date.UTC(2020, 0, 9))])
        .nice(5)
        .domain()
        .map((d) => d.getTime());
      return {
        scale: linear(50),
        niceDomain: linear.domain(),
        arrayNice: d3.nice(0, 97, 10),
        timeSpan: timeDomain[1] - timeDomain[0],
        arcStart: d3.arc().innerRadius(0).outerRadius(100)({ startAngle: 0, endAngle: Math.PI / 2 }).slice(0, 3),
        formatted: d3.format(",.1f")(1234.5),
      };
    `,
    );
    expect(result).toEqual({
      scale: 250,
      niceDomain: [0, 100],
      arrayNice: [0, 100],
      timeSpan: 691_200_000,
      arcStart: "M0,",
      formatted: "1,234.5",
    });
  });
});
