import type { ComponentProps } from "react";

import { cn } from "../../lib/utils.ts";
import { focusRing } from "./button.tsx";

/** The control styles text inputs and selects share; `aria-invalid` marks a rejected value. */
export const controlClasses = cn(
  "h-9 w-full min-w-0 rounded-md border border-input bg-background px-3 py-1 text-base shadow-xs transition-colors md:text-sm",
  "placeholder:text-muted-foreground disabled:cursor-not-allowed disabled:opacity-50",
  "aria-invalid:border-destructive",
  focusRing,
);

export function Input({ className, ...props }: ComponentProps<"input">) {
  return <input data-slot="input" className={cn(controlClasses, className)} {...props} />;
}
