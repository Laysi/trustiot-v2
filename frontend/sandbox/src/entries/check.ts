// check.js: the checker's host page script. It defines `tqCheckHost` for the checking service to drive.
import { createCheckHost } from "../check-host.ts";

Object.defineProperty(globalThis, "tqCheckHost", { value: createCheckHost(window), configurable: true });
