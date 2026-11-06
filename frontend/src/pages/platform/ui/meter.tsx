import { cn } from "../../../lib/utils.ts";

/**
 * A thin bar of how much of a limit is used. A `<progress>` element, so the width needs no
 * `style` attribute (the CSP forbids them, frontend/README.md 「CSP 的限制」) and a screen reader
 * gets the value; callers also print the numbers next to it. At 80% the bar is amber, past the
 * limit red.
 */
export function Meter({
  value,
  max,
  label,
  className,
}: {
  value: number;
  /** The limit; `0` is full, because nothing is allowed. */
  max: number;
  label: string;
  className?: string;
}) {
  const full = max <= 0;
  const ratio = full ? 1 : value / max;
  const tone = ratio > 1 ? "destructive" : ratio >= 0.8 ? "warning" : "primary";
  return (
    <progress
      aria-label={label}
      value={Math.min(full ? 1 : value, full ? 1 : max)}
      max={full ? 1 : max}
      data-tone={tone}
      className={cn(
        "block h-1.5 w-full appearance-none overflow-hidden rounded-full bg-muted",
        "[&::-webkit-progress-bar]:bg-muted [&::-moz-progress-bar]:rounded-full",
        tone === "destructive" &&
          "text-destructive [&::-webkit-progress-value]:bg-destructive [&::-moz-progress-bar]:bg-destructive",
        tone === "warning" && "[&::-webkit-progress-value]:bg-warning [&::-moz-progress-bar]:bg-warning",
        tone === "primary" && "[&::-webkit-progress-value]:bg-primary [&::-moz-progress-bar]:bg-primary",
        className,
      )}
    />
  );
}
