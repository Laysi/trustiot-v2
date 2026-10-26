// runtime.js: the sandbox runtime. It starts only in the sandbox document (an opaque-origin frame).
import { installSandbox } from "../runtime.ts";

installSandbox(window);
