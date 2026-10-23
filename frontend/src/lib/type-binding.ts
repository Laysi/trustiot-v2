/**
 * Type-level checks that bind a hand-written zod schema to the types generated from
 * api/openapi.json (src/api/schema.gen.ts), so a change in the spec fails `typecheck`:
 *
 *     export type TenantMatchesSpec = Assert<Equal<z.output<typeof Tenant>, components["schemas"]["Tenant"]>>;
 *
 * `satisfies z.ZodType<Spec>` alone checks one direction: a field the spec dropped, or made
 * optional, still passes. `Equal` is true only for identical types, so it fails both ways.
 */
export type Equal<A, B> = (<T>() => T extends A ? 1 : 2) extends <T>() => T extends B ? 1 : 2 ? true : false;

/** Compiles only when `T` is `true`. */
export type Assert<T extends true> = T;

/** `true` when every member of the tuple is `true`; an intersection of the checks would hide a `false`. */
export type AllTrue<T extends readonly boolean[]> = false extends T[number] ? false : true;
