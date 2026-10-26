// Preloaded by bun test (bunfig.toml): a DOM for the component tests. Tests that touch the network
// or time pass in their own fetch and clock instead of relying on these globals.
import { mock } from "bun:test";
import { GlobalRegistrator } from "@happy-dom/global-registrator";

import * as noStyleSingleton from "../lib/no-style-singleton.ts";

GlobalRegistrator.register({ url: "https://app.example.test/" });

// The build replaces react-style-singleton with a version that writes no <style> element
// (vite.config.ts, src/lib/no-style-singleton.ts); the tests run the same code as the build.
mock.module("react-style-singleton", () => noStyleSingleton);
