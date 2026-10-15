import { describe, expect, test } from "bun:test";
import { BUILDER_PACKAGES, parseBuilder, parseDpkgVersions } from "./lib.ts";
import { RUST_IMAGE_DRIVER, imageStore, requireStore } from "./pack.ts";

describe("imageStore", () => {
  test("a vfs store whose graph root and run root are inside its own directory, rootless too", () => {
    const store = imageStore("/tmp/trustiot-release-x1/store", RUST_IMAGE_DRIVER);
    expect(RUST_IMAGE_DRIVER).toBe("vfs");
    expect(store.graphRoot).toBe("/tmp/trustiot-release-x1/store/graph");
    expect(store.env).toEqual({ CONTAINERS_STORAGE_CONF: "/tmp/trustiot-release-x1/store/storage.conf" });
    expect(store.confText).toBe(
      [
        "[storage]",
        'driver = "vfs"',
        'graphroot = "/tmp/trustiot-release-x1/store/graph"',
        'rootless_storage_path = "/tmp/trustiot-release-x1/store/graph"',
        'runroot = "/tmp/trustiot-release-x1/store/run"',
        "",
      ].join("\n"),
    );
  });

  test("refuses paths and drivers that storage.conf could not hold as they are", () => {
    expect(() => imageStore("store", "vfs")).toThrow("absolute path");
    expect(() => imageStore('/tmp/a"b', "vfs")).toThrow("plain characters");
    expect(() => imageStore("/tmp/a b", "vfs")).toThrow("plain characters");
    expect(() => imageStore("/tmp/a", 'vfs"\nx')).toThrow("not a storage driver");
  });

  test("requireStore accepts only the store and driver podman was given", () => {
    const store = imageStore("/tmp/trustiot-release-x1/store", "vfs");
    expect(() => requireStore({ store: { graphRoot: store.graphRoot, graphDriverName: "vfs" } }, store)).not.toThrow();
    // podman 4.9.3, rootless, with only graphroot set: its database in the default store wins.
    expect(() => requireStore({ store: { graphRoot: "/home/u/.local/share/containers/storage", graphDriverName: "overlay" } }, store)).toThrow(
      "not /tmp/trustiot-release-x1/store/graph with vfs",
    );
    expect(() => requireStore({ store: { graphRoot: store.graphRoot, graphDriverName: "overlay" } }, store)).toThrow("with the overlay driver");
    expect(() => requireStore({}, store)).toThrow("podman uses the store undefined");
  });
});

describe("the builder recorded in build.json", () => {
  test("parseDpkgVersions keeps the known packages that are installed", () => {
    const output = [
      "podman 4.9.3+ds1-1ubuntu0.2",
      "buildah 1.33.7+ds1-1ubuntu0.24.04.3",
      "crun ",
      "golang-github-containers-image 5.29.2-2",
      "libc6 2.39-0ubuntu8",
      "skopeo 1.13.3 extra",
      "",
    ].join("\n");
    expect(parseDpkgVersions(output)).toEqual({
      podman: "4.9.3+ds1-1ubuntu0.2",
      buildah: "1.33.7+ds1-1ubuntu0.24.04.3",
      "golang-github-containers-image": "5.29.2-2",
    });
    expect(BUILDER_PACKAGES).toContain("podman");
    expect(BUILDER_PACKAGES).toContain("buildah");
  });

  test("parseBuilder passes versions through and nothing else", () => {
    const builder = { podman: "4.9.3", buildah: "1.33.7", packages: { podman: "4.9.3+ds1-1ubuntu0.2" } };
    expect(parseBuilder(builder)).toEqual(builder);
    expect(parseBuilder({ ...builder, packages: {} })).toEqual({ ...builder, packages: {} });
    expect(parseBuilder({ ...builder, extra: "ignored" })).toEqual(builder);
    for (const bad of [
      undefined,
      [],
      { ...builder, podman: "4.9.3 [link](https://example.com)" },
      { ...builder, buildah: "" },
      { ...builder, podman: 4 },
      { ...builder, packages: [] },
      { ...builder, packages: { libc6: "2.39" } },
      { ...builder, packages: { podman: "`x`" } },
    ]) {
      expect(() => parseBuilder(bad)).toThrow("no valid builder");
    }
  });
});
