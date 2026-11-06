/** A cropped area of a page image, in the image's pixels. */
export interface Box {
  x: number;
  y: number;
  width: number;
  height: number;
}

/** A whole number of at least 1 from a query parameter; `null` for anything else. */
export function positiveInteger(raw: string | null): number | null {
  if (raw === null || !/^[0-9]{1,9}$/.test(raw)) return null;
  const value = Number(raw);
  return value >= 1 ? value : null;
}

/**
 * The `box=x,y,w,h` of a citation as an area inside an image of `width` by `height`: four finite
 * numbers, a positive size and a corner inside the image; the part outside is cut off. Anything
 * else is no box, as the other parameters of the design's deep links (an unknown value falls back).
 */
export function parseBox(raw: string | null, width: number, height: number): Box | null {
  if (raw === null) return null;
  const parts = raw.split(",");
  if (parts.length !== 4 || !parts.every((part) => /^-?[0-9]+(\.[0-9]+)?$/.test(part.trim()))) return null;
  const [x, y, w, h] = parts.map((part) => Number(part.trim())) as [number, number, number, number];
  if (![x, y, w, h].every(Number.isFinite) || x < 0 || y < 0 || w <= 0 || h <= 0 || x >= width || y >= height) {
    return null;
  }
  return { x, y, width: Math.min(w, width - x), height: Math.min(h, height - y) };
}
