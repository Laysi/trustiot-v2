// The only module that imports zod (biome.json forbids it everywhere else); the rest of the app
// imports `z` from here.
//
// zod 4 probes whether it may compile parsers when a `z.object` schema is created, by running
// `new Function("")`. The page's CSP has no 'unsafe-eval', so the probe throws (zod swallows it),
// but the browser still reports a `securitypolicyviolation`, and the README forbids `new
// Function`. `jitless` makes zod skip the probe and use its interpreted parsers. It has to be set
// before any schema is created: this module sets it, then exports `z`, and an ES module's body
// runs before the body of every module that imports it.
import { z } from "zod";

z.config({ jitless: true });

export { z };
