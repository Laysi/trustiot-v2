import { describe, expect, test } from "bun:test";
import { parseBox, positiveInteger } from "./box.ts";
import { formatBytes, isPdf } from "./format.ts";
import { documentPath } from "./paths.ts";

describe("the citation's box", () => {
  test("four numbers inside the page are the area, and the part outside is cut off", () => {
    expect(parseBox("60,190,674,160", 794, 1123)).toEqual({ x: 60, y: 190, width: 674, height: 160 });
    expect(parseBox("700, 1000, 300, 300", 794, 1123)).toEqual({ x: 700, y: 1000, width: 94, height: 123 });
    expect(parseBox("0.5,1.5,10.25,20", 794, 1123)).toEqual({ x: 0.5, y: 1.5, width: 10.25, height: 20 });
  });

  test("anything else is no box", () => {
    for (const raw of [
      null,
      "",
      "1,2,3",
      "1,2,3,4,5",
      "a,b,c,d",
      "-1,0,10,10",
      "0,0,0,10",
      "0,0,10,-5",
      "800,0,10,10",
      "0,1200,10,10",
      "1e3,0,1,1",
      "NaN,0,1,1",
    ]) {
      expect(parseBox(raw, 794, 1123)).toBeNull();
    }
  });

  test("a page or version number is a whole number of at least one", () => {
    expect(positiveInteger("12")).toBe(12);
    for (const raw of [null, "", "0", "-3", "1.5", "abc", "1e2", "99999999999"])
      expect(positiveInteger(raw)).toBeNull();
  });
});

describe("the library's helpers", () => {
  test("sizes are written in decimal units", () => {
    expect(formatBytes(0, "en")).toBe("0 B");
    expect(formatBytes(1_400_000_000, "en")).toBe("1.4 GB");
    expect(formatBytes(5_000_000_000, "zh-TW")).toBe("5 GB");
    expect(formatBytes(82_000_000, "en")).toBe("82 MB");
  });

  test("only PDFs are taken, by type or by name", () => {
    expect(isPdf(new File(["x"], "a.pdf", { type: "application/pdf" }))).toBe(true);
    expect(isPdf(new File(["x"], "A.PDF"))).toBe(true);
    expect(isPdf(new File(["x"], "panel-photo.jpg", { type: "image/jpeg" }))).toBe(false);
  });

  test("a deep link names the version, the page and the box, and encodes the ID", () => {
    expect(documentPath("a/b")).toBe("/documents/a%2Fb");
    expect(documentPath("x", { version: 2, page: 12, box: { x: 1, y: 2, width: 3, height: 4 } })).toBe(
      "/documents/x?version=2&page=12&box=1%2C2%2C3%2C4",
    );
  });
});
