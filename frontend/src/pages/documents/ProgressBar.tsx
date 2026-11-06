import { cn } from "../../lib/utils.ts";

/**
 * A native `<progress>`: the CSP forbids the `style` attribute a width bar would need, and the
 * browser reports the element as a progress bar by itself. Without `value` it is indeterminate.
 */
export function ProgressBar({
  label,
  value,
  max = 100,
  valueText,
  className,
}: {
  label: string;
  value?: number;
  max?: number;
  /** What a screen reader says instead of the number (`1.4 GB of 5 GB`). */
  valueText?: string;
  className?: string;
}) {
  return (
    <progress
      aria-label={label}
      aria-valuetext={valueText}
      {...(value === undefined ? {} : { value })}
      max={max}
      className={cn(
        "block h-1.5 w-full appearance-none overflow-hidden rounded-full bg-muted",
        "[&::-webkit-progress-bar]:bg-muted [&::-webkit-progress-value]:bg-primary [&::-moz-progress-bar]:bg-primary",
        className,
      )}
    />
  );
}
