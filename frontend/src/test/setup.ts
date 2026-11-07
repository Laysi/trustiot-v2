// Preloaded by bun test (bunfig.toml): a DOM for the component tests. Tests that touch the network
// or time pass in their own fetch and clock instead of relying on these globals.
import { beforeEach, mock } from "bun:test";
import { GlobalRegistrator } from "@happy-dom/global-registrator";

import * as noStyleSingleton from "../lib/no-style-singleton.ts";
import { installNodePrinter } from "./print-node.ts";

GlobalRegistrator.register({ url: "https://app.example.test/" });

// The build replaces react-style-singleton with a version that writes no <style> element
// (vite.config.ts, src/lib/no-style-singleton.ts); the tests run the same code as the build.
mock.module("react-style-singleton", () => noStyleSingleton);

// A node in a failed `expect` prints as its markup, not as everything it reaches (print-node.ts).
installNodePrinter();

// React Router's ScrollRestoration (app/Layout.tsx) keeps its saved positions in module state that
// every test file of a `bun test` process shares, and a memory router's first entry always has the
// key "default": a router mounting in one test would restore the position an earlier test left that
// entry at, with a router update and a scrollTo that depend on the order of the tests. It takes the
// positions from this session storage entry when it mounts, so every test starts with none.
beforeEach(() => {
  window.sessionStorage.setItem("react-router-scroll-positions", "{}");
});
