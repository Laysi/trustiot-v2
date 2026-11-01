// Constants of the dashboard sandbox's protocol that code outside the frontend loads at run time
// without the frontend's packages installed: tools/check-dist.ts (through tools/build-sandbox.ts),
// which the release scripts' tests import, and fixtures/expect.ts, which the dashboard check image's
// test imports. So this module imports nothing; protocol.ts re-exports both constants.

/** Runtime versions this build ships (`RuntimeVersion::SHIPPED`). */
export const RUNTIMES = [1] as const;

/** Most bytes of a page (`spec::MAX_PAGE_BYTES`), as UTF-8. */
export const MAX_PAGE_BYTES = 40 * 1024;
