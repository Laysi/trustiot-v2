import type { ComponentProps } from "react";

import { cn } from "../../lib/utils.ts";

/** A form label; `htmlFor` is required, so every label names the control it belongs to. */
export function Label({ className, htmlFor, children, ...props }: ComponentProps<"label"> & { htmlFor: string }) {
  return (
    <label
      data-slot="label"
      htmlFor={htmlFor}
      className={cn("text-sm leading-none font-medium select-none", className)}
      {...props}
    >
      {children}
    </label>
  );
}
