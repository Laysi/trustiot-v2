// Freezes this realm's ECMAScript intrinsics before any page script runs (frontend/sandbox/README.md,
// 「比對程式」). The display-time comparison, the formatters it calls (the bundled d3-format and
// d3-time-format), and the time, value and geometry code all look their built-ins up at call time:
// `RegExp.prototype.exec`, `String.prototype.replace`, `Array.from`, `Set.prototype.has`, `Math.*`,
// `Number.parse*`, `Intl.DateTimeFormat`, `Date.prototype.getTime`, and so on. If a page could
// replace one of those, the page's own `tq.text` and the comparison would format through the same
// replacement and agree on a wrong number. Freezing the intrinsics closes that: after `harden`,
// nothing a page does changes what the comparison computes, except changing the DOM, which the
// comparison re-reads.
//
// Scope: the standard-library objects reachable from the intrinsics seeded below, transitively, and
// the global bindings that point at them. The comparison reads an intrinsic both ways — through the
// object (`Math.round`, frozen here) and through the binding (bare `Math`, resolved on the global
// object) — so `harden` freezes the objects and makes each such global binding non-writable and
// non-configurable, or a page could swap the whole `Math` out from under a bare reference. It does
// not make the global object non-extensible (the runtime still defines `tq`, the page still adds its
// own globals) or touch any DOM or host binding (the page still builds its chart). The checker
// compares from a separate isolated world, whose intrinsics the page cannot reach, so it does not
// depend on this; `harden` protects the display-time comparison, which shares the page's realm.
//
// The "override mistake": once a prototype's data property is non-writable, assigning a property of
// that name on an object that inherits it throws in strict mode (`obj.toString = f`, `err.name =
// s`). `harden` turns every data property of the frozen prototypes into an accessor first, so such
// an assignment still creates an own property on the assignee — but the setter refuses to change
// the prototype's own slot, so a page cannot replace a method there. The genuine value is what the
// getter returns, which is what the comparison reads.

const {
  defineProperty,
  freeze,
  getOwnPropertyDescriptor,
  getOwnPropertyNames,
  getOwnPropertySymbols,
  getPrototypeOf,
  isFrozen,
  prototype: objectPrototype,
} = Object;

/** The value as an object (or function), or `null` for a primitive. */
function asObject(value: unknown): object | null {
  if ((typeof value !== "object" && typeof value !== "function") || value === null) return null;
  return value as object;
}

/** The intrinsic objects to freeze, seeded by name, then closed over their reachable members. */
function intrinsics(): { all: Set<object>; prototypes: Set<object> } {
  const all = new Set<object>();
  const prototypes = new Set<object>();
  const add = asObject;
  const seedPrototype = (ctor: unknown) => {
    const object = add(ctor);
    if (object === null) return;
    all.add(object);
    const proto = add((object as { prototype?: unknown }).prototype);
    if (proto !== null) {
      all.add(proto);
      prototypes.add(proto);
    }
  };

  // The primordials: Object and Function and their prototypes, reached without naming a global.
  seedPrototype(Object);
  seedPrototype(Function);
  // Every standard constructor the comparison path or d3 may touch, by name on the global. A name
  // the realm does not have (an older engine, a missing Intl) is simply skipped.
  const named = [
    "Array",
    "Boolean",
    "Number",
    "String",
    "Symbol",
    "BigInt",
    "Date",
    "RegExp",
    "Error",
    "EvalError",
    "RangeError",
    "ReferenceError",
    "SyntaxError",
    "TypeError",
    "URIError",
    "AggregateError",
    "Map",
    "Set",
    "WeakMap",
    "WeakSet",
    "Promise",
    "ArrayBuffer",
    "SharedArrayBuffer",
    "DataView",
    "Int8Array",
    "Uint8Array",
    "Uint8ClampedArray",
    "Int16Array",
    "Uint16Array",
    "Int32Array",
    "Uint32Array",
    "Float32Array",
    "Float64Array",
    "BigInt64Array",
    "BigUint64Array",
    // The global functions the comparison or d3 may call as bare references.
    "parseInt",
    "parseFloat",
    "isNaN",
    "isFinite",
    "decodeURI",
    "decodeURIComponent",
    "encodeURI",
    "encodeURIComponent",
  ] as const;
  const scope = globalThis as unknown as Record<string, unknown>;
  for (const name of named) seedPrototype(scope[name]);
  // The typed arrays share a hidden `%TypedArray%` constructor and prototype.
  const typed = add(scope.Uint8Array);
  if (typed !== null) {
    seedPrototype(getPrototypeOf(typed));
  }
  // The namespace objects: their own methods are what the comparison and d3 read (Math.round,
  // Number.parseInt is on Number itself, JSON.stringify, Reflect.apply, Intl.DateTimeFormat).
  for (const namespace of [Math, JSON, Reflect, scope.Intl]) {
    const object = add(namespace);
    if (object !== null) all.add(object);
  }
  // Intl's own constructors (DateTimeFormat, NumberFormat, ...) and their prototypes.
  const intl = add(scope.Intl);
  if (intl !== null) {
    for (const key of getOwnPropertyNames(intl)) {
      const value = (intl as Record<string, unknown>)[key];
      if (typeof value === "function") seedPrototype(value);
    }
  }
  // The iterator prototypes: `[][Symbol.iterator]()` reaches %ArrayIteratorPrototype% and, above
  // it, %IteratorPrototype%. Freezing them keeps `for...of` and spread genuine.
  const arrayIterator = add(getPrototypeOf(add([][Symbol.iterator]()) ?? {}));
  if (arrayIterator !== null) {
    all.add(arrayIterator);
    prototypes.add(arrayIterator);
    const iterator = add(getPrototypeOf(arrayIterator));
    if (iterator !== null) {
      all.add(iterator);
      prototypes.add(iterator);
    }
  }

  // Close the set over the objects the seeds reference: prototypes, and the values of their own
  // properties (so a prototype's methods and a constructor's static methods are frozen too). The
  // seeds are standard intrinsics, which reference only standard intrinsics, so the walk stays
  // inside the ECMAScript library and never reaches a DOM or host object.
  const stack = [...all];
  while (stack.length > 0) {
    const object = stack.pop();
    if (object === undefined) continue;
    const proto = add(getPrototypeOf(object));
    if (proto !== null && !all.has(proto)) {
      all.add(proto);
      stack.push(proto);
    }
    for (const key of [...getOwnPropertyNames(object), ...getOwnPropertySymbols(object)]) {
      const descriptor = getOwnPropertyDescriptor(object, key);
      if (descriptor === undefined) continue;
      for (const slot of [descriptor.value, descriptor.get, descriptor.set]) {
        const member = add(slot);
        if (member !== null && !all.has(member)) {
          all.add(member);
          stack.push(member);
        }
      }
    }
  }
  return { all, prototypes };
}

/**
 * Turns every data property of `prototype` into an accessor that returns the genuine value and, on
 * assignment, defines an own property on the assignee instead of the prototype — so inheriting
 * objects can still shadow the property (the override mistake stays fixed) while the prototype's
 * own slot cannot be changed. Accessor properties and `__proto__` are left as they are.
 */
function enableOverrides(prototype: object): void {
  for (const key of [...getOwnPropertyNames(prototype), ...getOwnPropertySymbols(prototype)]) {
    if (key === "__proto__") continue;
    const descriptor = getOwnPropertyDescriptor(prototype, key);
    if (descriptor === undefined || !("value" in descriptor) || !descriptor.configurable) continue;
    const { value } = descriptor;
    defineProperty(prototype, key, {
      get() {
        return value;
      },
      set(this: object, replacement: unknown) {
        // Assigning on the prototype itself does nothing: the page cannot replace the method
        // every inheritor (and the comparison) reads. Assigning on an inheritor shadows it there.
        if (this === prototype) return;
        defineProperty(this, key, { value: replacement, writable: true, enumerable: true, configurable: true });
      },
      enumerable: descriptor.enumerable,
      configurable: false,
    });
  }
}

/**
 * Makes every own property of the global object whose value is one of the frozen intrinsics (a bare
 * `Math`, `Array`, `Intl`, `parseInt`, ...) non-writable and non-configurable, so a page cannot
 * replace the binding a bare reference resolves to. The global object stays extensible, and
 * bindings to other values (the runtime's `tq`, the page's d3, DOM globals) are untouched.
 */
function lockGlobalBindings(all: Set<object>): void {
  const scope = globalThis;
  for (const key of getOwnPropertyNames(scope)) {
    const descriptor = getOwnPropertyDescriptor(scope, key);
    if (descriptor === undefined || !("value" in descriptor) || !descriptor.configurable) continue;
    const value = asObject(descriptor.value);
    if (value === null || !all.has(value)) continue;
    defineProperty(scope, key, {
      value: descriptor.value,
      writable: false,
      enumerable: descriptor.enumerable,
      configurable: false,
    });
  }
}

/**
 * Freezes this realm's ECMAScript intrinsics (and their prototypes and members) and locks the
 * global bindings that point at them, after making the prototypes' data properties overridable per
 * object. Idempotent, and safe to call in a realm whose intrinsics are already frozen. Call it once,
 * before any page script runs.
 */
export function harden(): void {
  // A realm that is already locked down (Object.prototype frozen) is left alone: re-running the
  // enablement would be refused on the now non-configurable properties.
  if (isFrozen(objectPrototype)) return;
  const { all, prototypes } = intrinsics();
  for (const prototype of prototypes) enableOverrides(prototype);
  for (const object of all) freeze(object);
  lockGlobalBindings(all);
}
