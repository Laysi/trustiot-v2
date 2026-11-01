// WCAG 2.2 contrast check of every colour pair in frontend/src/styles/global.css.
// Usage: bun contrast.ts <path to global.css>
import { readFileSync } from "node:fs";

type RGB = [number, number, number]; // linear-light sRGB
const clamp = (x: number) => Math.min(1, Math.max(0, x));
function oklchToLinear(l: number, c: number, hDeg: number): RGB {
  const h = (hDeg * Math.PI) / 180;
  const a = c * Math.cos(h);
  const b = c * Math.sin(h);
  const l_ = (l + 0.3963377774 * a + 0.2158037573 * b) ** 3;
  const m_ = (l - 0.1055613458 * a - 0.0638541728 * b) ** 3;
  const s_ = (l - 0.0894841775 * a - 1.291485548 * b) ** 3;
  return [
    clamp(4.0767416621 * l_ - 3.3077115913 * m_ + 0.2309699292 * s_),
    clamp(-1.2684380046 * l_ + 2.6097574011 * m_ - 0.3413193965 * s_),
    clamp(-0.0041960863 * l_ - 0.7034186147 * m_ + 1.707614701 * s_),
  ];
}
const toLin = (v: number) => (v <= 0.04045 ? v / 12.92 : ((v + 0.055) / 1.055) ** 2.4);
const toSrgb = (v: number) => (v <= 0.0031308 ? v * 12.92 : 1.055 * v ** (1 / 2.4) - 0.055);
function parse(value: string): RGB {
  const v = value.trim();
  const hex = v.match(/^#([0-9a-f]{6})$/i);
  if (hex) {
    const n = parseInt(hex[1] as string, 16);
    return [toLin(((n >> 16) & 255) / 255), toLin(((n >> 8) & 255) / 255), toLin((n & 255) / 255)];
  }
  const ok = v.match(/^oklch\(\s*([\d.]+)\s+([\d.]+)\s+([\d.]+)\s*\)$/);
  if (ok) return oklchToLinear(Number(ok[1]), Number(ok[2]), Number(ok[3]));
  throw new Error(`cannot parse ${v}`);
}
const lum = (c: RGB) => 0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2];
function ratio(a: RGB, b: RGB): number {
  const hi = Math.max(lum(a), lum(b));
  const lo = Math.min(lum(a), lum(b));
  return (hi + 0.05) / (lo + 0.05);
}
/** Alpha blend in gamma-encoded sRGB, the way browsers do it. */
function blend(fg: RGB, bg: RGB, alpha: number): RGB {
  const out = fg.map((v, i) => toLin(toSrgb(v) * alpha + toSrgb(bg[i] as number) * (1 - alpha)));
  return out as RGB;
}
/** OKLab lightness of a linear colour. */
function oklabL(c: RGB): number {
  const l = Math.cbrt(0.4122214708 * c[0] + 0.5363325363 * c[1] + 0.0514459929 * c[2]);
  const m = Math.cbrt(0.2119034982 * c[0] + 0.6806995451 * c[1] + 0.1073969566 * c[2]);
  const s = Math.cbrt(0.0883024619 * c[0] + 0.2817188376 * c[1] + 0.6299787005 * c[2]);
  return 0.2104542553 * l + 0.793617785 * m - 0.0040720468 * s;
}

const css = readFileSync(process.argv[2] as string, "utf8");
function block(selector: string): Record<string, string> {
  const start = css.indexOf(`${selector} {`);
  const body = css.slice(start, css.indexOf("}", start));
  const out: Record<string, string> = {};
  for (const m of body.matchAll(/--([a-z-]+):\s*([^;]+);/g)) out[m[1] as string] = (m[2] as string).trim();
  return out;
}
const light = block(":root");
const dark = { ...light, ...block(':root[data-theme="dark"]') };
const resolver = (t: Record<string, string>) => (name: string) => {
  let v = t[name] as string;
  const ref = v.match(/^var\(--([a-z-]+)\)$/);
  if (ref) v = t[ref[1] as string] as string;
  return parse(v);
};

const lines: string[] = [];
let failed = false;
function check(theme: string, label: string, fg: RGB, bg: RGB, min: number) {
  const r = ratio(fg, bg);
  const ok = r >= min;
  if (!ok) failed = true;
  lines.push(`| ${theme} | ${label} | ${r.toFixed(2)} | ${min} | ${ok ? "pass" : "FAIL"} |`);
}
const info = (theme: string, label: string, value: string) => lines.push(`| ${theme} | ${label} | ${value} | - | - |`);
const TEXT = 4.5;
const UI = 3;
for (const [theme, t] of [
  ["light", light],
  ["dark", dark],
] as const) {
  const c = resolver(t);
  for (const [fg, bg] of [
    ["foreground", "background"],
    ["card-foreground", "card"],
    ["popover-foreground", "popover"],
    ["secondary-foreground", "secondary"],
    ["muted-foreground", "muted"],
    ["muted-foreground", "background"],
    ["accent-foreground", "accent"],
    ["primary-foreground", "primary"],
    ["destructive-foreground", "destructive"],
    ["on-brand", "brand"],
    ["on-brand-strong", "brand-strong"],
    ["foreground", "brand-soft"],
  ] as const) {
    check(theme, `text ${fg} on ${bg}`, c(fg), c(bg), TEXT);
  }
  check(
    theme,
    "text primary-foreground on primary/90 over background (button hover)",
    c("primary-foreground"),
    blend(c("primary"), c("background"), 0.9),
    TEXT,
  );
  for (const s of ["background", "card", "popover", "secondary", "muted", "accent", "brand-soft"]) {
    check(theme, `text primary (link) on ${s}`, c("primary"), c(s), TEXT);
    check(theme, `focus ring on ${s}`, c("ring"), c(s), UI);
  }
  for (const s of ["background", "card", "popover"]) {
    check(theme, `text success on ${s}`, c("success"), c(s), TEXT);
    check(theme, `text destructive on ${s}`, c("destructive"), c(s), TEXT);
    check(theme, `input border on ${s}`, c("input"), c(s), UI);
  }
  check(theme, "chart line and points: primary on background (graphic)", c("primary"), c("background"), UI);
  info(theme, "chart band primary/15 over background vs background (tint)", ratio(blend(c("primary"), c("background"), 0.15), c("background")).toFixed(2));
  info(theme, "success vs primary: luminance contrast", ratio(c("success"), c("primary")).toFixed(2));
  info(theme, "success vs primary: OKLab lightness L (success / primary)", `${oklabL(c("success")).toFixed(2)} / ${oklabL(c("primary")).toFixed(2)}`);
  info(theme, "destructive vs primary: luminance contrast", ratio(c("destructive"), c("primary")).toFixed(2));
}
// The logo is one file (favicon.svg, also the layout mark): the website's mint #0d9b76, no tile.
const mark = parse("#0d9b76");
check("light", "logo mark #0d9b76 on white page (graphic)", mark, parse("#ffffff"), UI);
check("dark", "logo mark #0d9b76 on dark page background (graphic)", mark, resolver(dark)("background"), UI);
check("dark", "logo mark #0d9b76 on dark card (graphic)", mark, resolver(dark)("card"), UI);
// Browser tab strips differ by browser; reference values only, not pass/fail.
info("light", "logo mark on Chrome light active tab #ffffff / inactive tab strip #dee1e6", `${ratio(mark, parse("#ffffff")).toFixed(2)} / ${ratio(mark, parse("#dee1e6")).toFixed(2)}`);
info("dark", "logo mark on Chrome dark active tab #35363a / inactive tab strip #202124", `${ratio(mark, parse("#35363a")).toFixed(2)} / ${ratio(mark, parse("#202124")).toFixed(2)}`);
check("light", "text on-brand #03140f on brand #0d9b76", parse("#03140f"), mark, TEXT);
console.log("| 主題 | 配對 | 對比 | 門檻 | 結果 |\n| --- | --- | --- | --- | --- |");
console.log(lines.join("\n"));
console.log(failed ? "\nRESULT: FAIL" : "\nRESULT: all pairs pass");
process.exit(failed ? 1 : 0);
