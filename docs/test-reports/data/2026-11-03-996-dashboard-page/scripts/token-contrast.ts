// One-off contrast check for the tokens this change adds (WCAG 2.2 relative luminance).
function oklch(L: number, C: number, h: number): [number, number, number] {
  const a = C * Math.cos((h * Math.PI) / 180), b = C * Math.sin((h * Math.PI) / 180);
  const l_ = L + 0.3963377774 * a + 0.2158037573 * b, m_ = L - 0.1055613458 * a - 0.0638541728 * b, s_ = L - 0.0894841775 * a - 1.291485548 * b;
  const l = l_ ** 3, m = m_ ** 3, s = s_ ** 3;
  const lin = [4.0767416621 * l - 3.3077115913 * m + 0.2309699292 * s, -1.2684380046 * l + 2.6097574011 * m - 0.3413193965 * s, -0.0041960863 * l - 0.7034186147 * m + 1.707614701 * s];
  return lin.map((v) => Math.min(1, Math.max(0, v))) as [number, number, number];
}
function parse(c: string): [number, number, number] {
  if (c.startsWith("#")) {
    const n = parseInt(c.slice(1), 16);
    const f = (v: number) => { const x = v / 255; return x <= 0.04045 ? x / 12.92 : ((x + 0.055) / 1.055) ** 2.4; };
    return [f((n >> 16) & 255), f((n >> 8) & 255), f(n & 255)];
  }
  const m = /oklch\(([\d.]+) ([\d.]+) ([\d.]+)\)/.exec(c)!;
  return oklch(+m[1], +m[2], +m[3]);
}
const lum = (c: string) => { const [r, g, b] = parse(c); return 0.2126 * r + 0.7152 * g + 0.0722 * b; };
const ratio = (a: string, b: string) => { const [x, y] = [lum(a), lum(b)].sort((p, q) => q - p); return (x + 0.05) / (y + 0.05); };
const themes = {
  light: { warning: "#9d6304", soft: "#fef4df", card: "oklch(1 0 0)", background: "oklch(1 0 0)", muted: "oklch(0.968 0.004 265)", accent: "oklch(0.955 0.01 260)", mutedfg: "oklch(0.5 0.02 265)", fg: "oklch(0.21 0.015 265)", primary: "#086650", secondary: "oklch(0.968 0.004 265)", ring: "#086650", brandsoft: "#dcefe6", popover: "oklch(1 0 0)", success: "oklch(0.5 0.13 150)" },
  dark: { warning: "#f7b83d", soft: "#3f2903", card: "oklch(0.205 0.014 265)", background: "oklch(0.17 0.012 265)", muted: "oklch(0.27 0.014 265)", accent: "oklch(0.29 0.016 265)", mutedfg: "oklch(0.72 0.015 265)", fg: "oklch(0.96 0.005 265)", primary: "oklch(0.87 0.09 168)", secondary: "oklch(0.27 0.014 265)", ring: "oklch(0.87 0.09 168)", brandsoft: "#0e2d25", popover: "oklch(0.225 0.014 265)", success: "oklch(0.75 0.14 150)" },
} as const;
const rows: string[] = [];
for (const [name, t] of Object.entries(themes)) {
  const pairs: [string, string, string, number][] = [
    ["--warning / --warning-soft (badge text)", t.warning, t.soft, 4.5],
    ["--warning / --card (text)", t.warning, t.card, 4.5],
    ["--warning / --background (text)", t.warning, t.background, 4.5],
    ["--muted-foreground / --muted (stale badge text)", t.mutedfg, t.muted, 4.5],
    ["--success / --secondary (published icon)", t.success, t.secondary, 3],
    ["--muted-foreground / --accent (menu, hint text)", t.mutedfg, t.accent, 4.5],
    ["--foreground / --accent (current menu item text)", t.fg, t.accent, 4.5],
    ["--primary / --accent (current menu item bar and icon)", t.primary, t.accent, 3],
    ["--primary / --popover (menu icon, link)", t.primary, t.popover, 4.5],
    ["--ring / --background (focus ring)", t.ring, t.background, 3],
  ];
  for (const [label, a, b, need] of pairs) {
    const r = ratio(a, b);
    rows.push(`| ${name} | ${label} | ${r.toFixed(2)} | ${need} | ${r >= need ? "pass" : "FAIL"} |`);
  }
}
console.log(rows.join("\n"));
